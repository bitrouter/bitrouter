//! Bounded single-writer actor; admitted commands outlive HTTP cancellation.

#[cfg(test)]
mod tests;

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::tenancy::{
    operation::{ErrorCode, OperationResult, OperationStatus},
    validation::Validate,
};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use tokio::sync::{mpsc, oneshot};

use super::{
    command::Command,
    error::Failure,
    model::{Caller, Snapshot},
    mutation,
    store::{self, Operation, StoredResponse},
};

pub(super) struct Message {
    pub caller: Caller,
    pub command: Command,
    pub reply: oneshot::Sender<Result<StoredResponse, Failure>>,
}

/// Any exit (including task cancellation) invalidates the last published view.
pub(super) struct Liveness(pub Arc<AtomicBool>);
impl Drop for Liveness {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) struct Worker {
    pub db: DatabaseConnection,
    pub snapshot: Arc<RwLock<Arc<Snapshot>>>,
    pub live: Liveness,
    pub owner: Option<super::ownership::Lease>,
    pub limits: OperationLimits,
    pub resources: Arc<super::resources::Resources>,
}

#[derive(Clone, Copy)]
pub(super) struct OperationLimits {
    pub ordinary: u32,
    pub safety: u32,
}

impl Default for OperationLimits {
    fn default() -> Self {
        Self {
            ordinary: store::ORDINARY_OPERATIONS,
            safety: store::SAFETY_OPERATIONS,
        }
    }
}

impl Worker {
    pub async fn run(
        self,
        mut ordinary: mpsc::Receiver<Message>,
        mut safety: mpsc::Receiver<Message>,
        stop: tokio_util::sync::CancellationToken,
    ) {
        // The file remains owned until all admitted commands have settled.
        let _owner = &self.owner;
        let mut draining = false;
        let mut ordinary_open = true;
        let mut safety_open = true;
        loop {
            let message = tokio::select! {
                biased;
                () = stop.cancelled(), if !draining && (ordinary_open || safety_open) => {
                    ordinary.close();
                    safety.close();
                    draining = true;
                    continue;
                }
                message = safety.recv(), if safety_open => match message {
                    Some(message) => message,
                    None => {
                        safety_open = false;
                        continue;
                    }
                },
                message = ordinary.recv(), if ordinary_open => match message {
                    Some(message) => message,
                    None => {
                        ordinary_open = false;
                        continue;
                    }
                },
                else => break,
            };
            let writing = AtomicBool::new(false);
            let response = {
                // The future stays in this task. Dropping read-only preparation
                // cannot detach a continuation that later enters store::commit.
                let work = self.execute(&message.caller, &message.command, &writing);
                tokio::pin!(work);
                tokio::select! {
                    biased;
                    () = tokio::time::sleep(super::MANAGEMENT_TIMEOUT) => {
                        if writing.load(Ordering::Acquire) {
                            // Once commit starts, its outcome may be unknown.
                            // Fence admission and hold the owner until it settles.
                            self.live.0.store(false, Ordering::Release);
                            let _settled = work.await;
                        }
                        Err(Failure::new(ErrorCode::AuthorityUnavailable, Some(message.command.operation_id().clone())))
                    }
                    response = &mut work => response,
                }
                // A timed-out preparation is dropped before replying or taking
                // another command; it never publishes its private candidate.
            };
            if message.reply.send(response).is_err() {
                // A disconnected caller recovers via its operation ID; do not
                // retry the command or retain a one-time secret in diagnostics.
                tracing::debug!("tenancy command completed without a response receiver");
            }
        }
    }

    fn current(&self) -> Result<Arc<Snapshot>, Failure> {
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.snapshot
            .read()
            .map(|state| Arc::clone(&state))
            .map_err(|_| ErrorCode::AuthorityUnavailable.into())
    }

    fn unavailable(&self, code: ErrorCode) -> Failure {
        self.live.0.store(false, Ordering::Release);
        code.into()
    }

    async fn execute(
        &self,
        caller: &Caller,
        command: &Command,
        writing: &AtomicBool,
    ) -> Result<StoredResponse, Failure> {
        let old = self.current()?;
        let now = Utc::now();
        if command.installation() != &old.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let principal = old.principal(caller, now).map_err(Failure::from)?.clone();
        let digest = command
            .digest(&principal)
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        let previous = store::operation(
            &self.db,
            &old.installation,
            &principal,
            command.operation_id(),
        )
        .await
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
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
            return Ok(previous.response);
        }
        let existing_target = match command {
            Command::Tenant(id, _) => old.tenants.contains_key(id),
            _ => true,
        };
        let safety = old.saturated
            && matches!(caller, Caller::Installation(_))
            && command.safety()
            && existing_target;
        if (old.saturated && !safety) || (safety && old.safety_operation_ids >= self.limits.safety)
        {
            return Err(Failure::new(
                ErrorCode::CapacityExceeded,
                Some(command.operation_id().clone()),
            ));
        }
        let mut candidate = (*old).clone();
        candidate
            .credentials
            .retain(|_, credential| credential.expires.instant > now);
        candidate
            .credential_hashes
            .retain(|_, id| candidate.credentials.contains_key(id));
        // Management reads may have waited on storage. Expiry and lifecycle are
        // checked once more at application, under the actor's current authority.
        let now = Utc::now();
        let prepared =
            mutation::prepare(&self.db, &candidate, &self.resources, caller, command, now)
                .await
                .map_err(|code| {
                    if matches!(
                        code,
                        ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable
                    ) {
                        self.unavailable(code)
                    } else {
                        code.into()
                    }
                })?;
        let now = Utc::now();
        let outcome = mutation::apply(&mut candidate, caller, &principal, command, &prepared, now);
        let (changes, result, mut response) = match outcome {
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
                candidate = (*old).clone();
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
            candidate.safety_operation_ids += 1;
        } else {
            candidate.operation_ids += 1;
        }
        candidate.saturated |= candidate.operation_ids >= self.limits.ordinary;
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
        // Allocate the publication before committing. After commit there is no
        // await between replacing the immutable snapshot and acknowledging it.
        let next = Arc::new(candidate);
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        // No await or detached work may separate this marker from commit. The
        // watchdog must never cancel a potentially durable mutation as a read.
        writing.store(true, Ordering::Release);
        if store::commit(&self.db, &next, &changes, &operation)
            .await
            .is_err()
        {
            return Err(self.unavailable(ErrorCode::StorageUnavailable));
        }
        match self.snapshot.write() {
            Ok(mut state) => *state = next,
            Err(_) => {
                if let StoredResponse::Issued(issued) = &mut response {
                    issued.secret = None;
                }
                return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
            }
        }
        if self.live.0.load(Ordering::Acquire) {
            Ok(response)
        } else {
            Err(ErrorCode::AuthorityUnavailable.into())
        }
    }
}
