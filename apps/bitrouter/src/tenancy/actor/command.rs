//! Indexed command-local authority and original-actor transaction custody.

use super::*;
use crate::tenancy::{
    mutation,
    store::{Change, Operation},
    transaction::Disposition,
};
use bitrouter_ai::tenancy::{
    operation::{OperationResult, OperationStatus},
    validation::Validate,
};
use chrono::Utc;

enum Prepared {
    Replay {
        response: Box<StoredResponse>,
        facts: Box<Snapshot>,
        context: crate::tenancy::model::ReadContext,
    },
    Write(Box<Write>),
}

struct Write {
    next: Snapshot,
    changes: Vec<Change>,
    operation: Operation,
    response: StoredResponse,
    replaced_bytes: usize,
}

impl Worker {
    pub(super) async fn execute(
        &self,
        caller: &Caller,
        command: &Command,
        control: &Control,
    ) -> Result<StoredResponse, Failure> {
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        // The same actor serializes both phases. No authority writer can run
        // between preparation rollback and write BEGIN.
        control.prepare_read();
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let prepared = self
            .prepare_command(
                caller,
                command,
                &crate::tenancy::transaction::ReadConnection::new(&tx, control),
                control,
            )
            .await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        control
            .finish_preparation()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        let prepared = prepared?;
        let write = match prepared {
            Prepared::Replay {
                response,
                facts,
                context,
            } => {
                facts
                    .authorize_read(caller, &context, Utc::now())
                    .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
                return Ok(*response);
            }
            Prepared::Write(write) => write,
        };
        let Write {
            next,
            changes,
            operation,
            response,
            replaced_bytes,
        } = *write;
        let publication = self
            .authority
            .prepare_with_admission(
                next.header(),
                &changes,
                None,
                Some(next.resource_admission.clone()),
            )
            .map_err(Failure::from)?;
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        let encoded = crate::tenancy::storage::encoded_size(&(&changes, &operation))
            .ok()
            .and_then(|bytes| bytes.checked_add(replaced_bytes))
            .ok_or(ErrorCode::CapacityExceeded)?;
        control.reserve_write(encoded, command.safety())?;
        store::commit(&self.db, &next, &changes, &operation, control)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        if self.proof.disposition() != Disposition::Committed {
            self.proof.quarantine();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if publication.publish().is_err() {
            self.proof.quarantine();
            return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
        }
        if self.live.0.load(Ordering::Acquire) {
            Ok(response)
        } else {
            Err(ErrorCode::AuthorityUnavailable.into())
        }
    }

    async fn prepare_command(
        &self,
        caller: &Caller,
        command: &Command,
        tx: &impl sea_orm::ConnectionTrait,
        control: &Control,
    ) -> Result<Prepared, Failure> {
        control
            .check_read()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let authority = self.authority.read().map_err(Failure::from)?.snapshot();
        let header = authority.header();
        let facts = store::directory::load_command_in(
            tx,
            &header,
            caller,
            command,
            self.authority.load_budget() / 3,
        )
        .await
        .map_err(|error| {
            if control.check_read().is_err() {
                ErrorCode::CapacityExceeded.into()
            } else {
                self.directory_failure(error)
            }
        })?;
        let mut old = facts.snapshot;
        old.resource_admission = authority.resource_admission;
        // Upserts can replace larger old rows. Include the bounded command-local
        // facts rather than estimating only the new payload or directory count.
        let replaced_bytes = crate::tenancy::storage::encoded_size(&(
            &old.tenants,
            &old.principals,
            &old.credentials,
        ))
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        // Bindings use composite map keys, which JSON objects cannot encode.
        // Count the stored records themselves, including their embedded IDs.
        let replaced_bytes = old
            .bindings
            .values()
            .try_fold(replaced_bytes, |total, binding| {
                crate::tenancy::storage::encoded_size(binding)
                    .ok()
                    .and_then(|bytes| total.checked_add(bytes))
                    .ok_or(ErrorCode::CapacityExceeded)
            })?;
        let now = Utc::now();
        if command.installation() != &old.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let principal = old.principal(caller, now).map_err(Failure::from)?.clone();
        let digest = command
            .digest(&principal)
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        // A write-only credential must not leave a public offline guessing hash
        // in operation storage. Bind resource command digests to the wrapping key.
        let digest = self
            .resources
            .management_digest(command, &digest)
            .map_err(Failure::from)?;
        let previous = store::operation(tx, &old.installation, &principal, command.operation_id())
            .await
            .map_err(|_| {
                if control.check_read().is_err() {
                    ErrorCode::CapacityExceeded.into()
                } else {
                    self.unavailable(ErrorCode::StorageUnavailable)
                }
            })?;
        if let Some(previous) = previous {
            old.authorize_read(caller, &previous.context, Utc::now())
                .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
            if previous.created_at + chrono::Duration::days(30) <= now {
                return Err(Failure::new(
                    ErrorCode::OperationExpired,
                    Some(command.operation_id().clone()),
                ));
            }
            if previous.digest != digest {
                return Err(Failure::new(
                    ErrorCode::OperationConflict,
                    Some(command.operation_id().clone()),
                ));
            }
            return Ok(Prepared::Replay {
                response: Box::new(previous.response),
                facts: Box::new(old),
                context: previous.context,
            });
        }
        let existing_target = match command {
            Command::Tenant(id, _) => old.tenants.contains_key(id),
            _ => true,
        };
        let safety = old.saturated
            && matches!(caller, Caller::Installation(_))
            && command.safety()
            && existing_target;
        if old.saturated && !safety {
            return Err(Failure::new(
                ErrorCode::CapacityExceeded,
                Some(command.operation_id().clone()),
            ));
        }
        let mut candidate = old.clone();
        candidate
            .credentials
            .retain(|_, credential| credential.expires.instant > now);
        candidate
            .credential_hashes
            .retain(|_, id| candidate.credentials.contains_key(id));
        // Management reads may have waited on storage. Expiry and lifecycle are
        // checked once more at application, under the actor's current authority.
        let now = Utc::now();
        let prepared = mutation::prepare(tx, &candidate, &self.resources, caller, command, now)
            .await
            .map_err(|code| {
                if control.check_read().is_err() {
                    ErrorCode::CapacityExceeded.into()
                } else if matches!(
                    code,
                    ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable
                ) {
                    self.unavailable(code)
                } else {
                    code.into()
                }
            })?;
        if let Some(configuration) = &prepared.configuration {
            store::directory::extend_dependencies_in(
                tx,
                &mut candidate,
                &configuration.dependencies,
                self.authority.load_budget() / 3,
            )
            .await
            .map_err(|error| {
                if control.check_read().is_err() {
                    ErrorCode::CapacityExceeded.into()
                } else {
                    self.directory_failure(error)
                }
            })?;
        }
        control
            .check_read()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let now = Utc::now();
        let outcome = mutation::apply(&mut candidate, caller, &principal, command, &prepared, now);
        let (changes, result, response) = match outcome {
            Ok(applied) => {
                candidate.revision = old
                    .revision
                    .get()
                    .checked_add(1)
                    .and_then(|value| value.try_into().ok())
                    .ok_or_else(|| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                let result = OperationResult {
                    operation_id: command.operation_id().clone(),
                    status: OperationStatus::Applied,
                    authority_revision: Some(candidate.revision),
                    resource_revision: Some(applied.revision),
                    resource_id: Some(applied.resource),
                    error: None,
                };
                let response = applied.issued.map_or_else(
                    || StoredResponse::Operation(result.clone()),
                    |mut issued| {
                        issued.authority_revision = candidate.revision;
                        StoredResponse::Issued(issued)
                    },
                );
                let response = applied
                    .configuration
                    .map_or(response, StoredResponse::Configuration);
                (applied.changes, result, response)
            }
            Err(code) => {
                candidate = old.clone();
                let failure = Failure::new(code, Some(command.operation_id().clone())).0;
                let result = OperationResult {
                    operation_id: command.operation_id().clone(),
                    status: OperationStatus::Rejected,
                    authority_revision: Some(old.revision),
                    resource_revision: None,
                    resource_id: None,
                    error: Some(failure.clone()),
                };
                (Vec::new(), result, StoredResponse::Failure(failure))
            }
        };
        if safety {
            candidate.safety_operation_ids = candidate
                .safety_operation_ids
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        } else {
            candidate.operation_ids = candidate
                .operation_ids
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        }
        result
            .validate()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        let operation = Operation {
            principal,
            id: command.operation_id().clone(),
            context: prepared.context,
            digest,
            result,
            response: response.without_secret(),
            created_at: now,
            safety,
        };
        Ok(Prepared::Write(Box::new(Write {
            next: candidate,
            changes,
            operation,
            response,
            replaced_bytes,
        })))
    }
}
