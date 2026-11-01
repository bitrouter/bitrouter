//! Indexed authentication under the existing read-transaction supervisor.

use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::ResourceRef};

use super::{AuthorityRead, AuthoritySubject, Control, ErrorCode, Failure, Worker};
use crate::tenancy::{
    authority::live::{Authenticated, OAuthFact, View},
    model::Caller,
    oauth,
    store::directory,
};

impl Worker {
    pub(super) async fn read_authority(
        &self,
        read: &AuthorityRead,
        control: &Control,
    ) -> Result<Authenticated, Failure> {
        if !self.live.0.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        let (header, publication) = {
            let guard = self.authority.read().map_err(Failure::from)?;
            (guard.header().clone(), guard.publication())
        };
        let bytes = self.authority.load_budget();
        let references = self.read_dependencies(&read.subject, bytes / 3)?;
        let transaction = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            let caller = match &read.subject {
                AuthoritySubject::Token(hash) => {
                    let credential =
                        directory::credential_by_hash_in(&transaction, hash, bytes / 96)
                            .await
                            .map_err(|error| self.directory_failure(error))?
                            .ok_or(ErrorCode::Unauthorized)?;
                    Caller::Credential(credential.id)
                }
                AuthoritySubject::Caller(caller)
                | AuthoritySubject::Tenant { caller, .. }
                | AuthoritySubject::Extend { caller, .. } => caller.clone(),
            };
            let mut facts = match &read.subject {
                AuthoritySubject::Tenant { tenant, active, .. } => {
                    directory::load_tenant_in(
                        &transaction,
                        &header,
                        &caller,
                        tenant,
                        *active,
                        bytes / 3,
                    )
                    .await
                }
                _ => directory::load_caller_in(&transaction, &header, &caller, bytes / 3).await,
            }
            .map_err(|error| self.directory_failure(error))?;
            if let AuthoritySubject::Tenant {
                resource: Some(reference),
                ..
            } = &read.subject
            {
                directory::load_dependencies_in(
                    &transaction,
                    &mut facts,
                    std::slice::from_ref(reference),
                    bytes / 3,
                )
                .await
                .map_err(|error| self.directory_failure(error))?;
                if let Some(head) = crate::tenancy::resources::lifecycle::head(
                    &transaction,
                    &header.installation,
                    &reference.key(),
                )
                .await
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?
                {
                    if head.authority_revision > header.revision {
                        return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
                    }
                    facts.snapshot.resource_lifecycle.insert(head);
                }
            }
            facts
                .snapshot
                .principal(&caller, chrono::Utc::now())
                .map_err(Failure::from)?;
            directory::load_dependencies_in(&transaction, &mut facts, &references, bytes / 3)
                .await
                .map_err(|error| self.directory_failure(error))?;
            // Retain only metadata. The canonical reader validates the bounded
            // encrypted row and drops its payload before reading the next slot.
            let oauth_count = references
                .iter()
                .filter(|reference| reference.resource_kind == ResourceKind::Credential)
                .count();
            let oauth_bytes = oauth_count
                .checked_mul(32 * 1024)
                .and_then(|n| {
                    n.checked_add(if oauth_count == 0 {
                        0
                    } else {
                        4 * oauth::profile::PAYLOAD_BYTES
                    })
                })
                .ok_or(ErrorCode::CapacityExceeded)?;
            if oauth_bytes > bytes / 3 {
                return Err(ErrorCode::CapacityExceeded.into());
            }
            let mut oauth = Vec::new();
            oauth
                .try_reserve_exact(oauth_count)
                .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
            for reference in &references {
                control
                    .check_read()
                    .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
                if reference.resource_kind != ResourceKind::Credential {
                    continue;
                }
                let row = oauth::storage::lookup(&transaction, &header.installation, reference)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                oauth.push(OAuthFact {
                    reference: reference.clone(),
                    slot: oauth::storage::slot(&header.installation, reference)
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?,
                    metadata: row.map(|row| row.metadata),
                });
            }
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            let view = View::new(facts.snapshot, oauth).map_err(Failure::from)?;
            Ok::<_, Failure>((caller, view))
        }
        .await;
        // A cancelled waiter still owns its permit and transaction until this
        // acknowledged rollback. Registration never runs with SQL in flight.
        control
            .rollback(transaction)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let (caller, view) = result?;
        if !self.live.0.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        let lease = if let AuthoritySubject::Extend { lease, .. } = &read.subject {
            self.authority
                .replace(lease, view, publication)
                .map_err(Failure::from)?;
            lease.clone()
        } else {
            self.authority
                .register(view, publication)
                .map_err(Failure::from)?
        };
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&lease, |view| {
                view.facts()
                    .principal(&caller, chrono::Utc::now())
                    .map(|_| ())
            })
            .map_err(Failure::from)?;
        Ok(Authenticated {
            caller,
            lease,
            observed_revision: header.revision,
        })
    }

    fn read_dependencies(
        &self,
        subject: &AuthoritySubject,
        bytes: usize,
    ) -> Result<Vec<ResourceRef>, Failure> {
        let AuthoritySubject::Extend {
            lease,
            dependencies,
            ..
        } = subject
        else {
            return Ok(Vec::new());
        };
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(lease, |view| {
                let lifecycle = &view.facts().resource_lifecycle;
                let count = lifecycle
                    .records()
                    .count()
                    .checked_add(lifecycle.absences().count())
                    .and_then(|n| n.checked_add(dependencies.len()))
                    .ok_or(ErrorCode::CapacityExceeded)?;
                // Three bounded opaque IDs, the vector entry and sort workspace.
                if count
                    .checked_mul(512 + std::mem::size_of::<ResourceRef>())
                    .is_none_or(|n| n > bytes)
                {
                    return Err(ErrorCode::CapacityExceeded);
                }
                let mut references = Vec::new();
                references
                    .try_reserve_exact(count)
                    .map_err(|_| ErrorCode::CapacityExceeded)?;
                references.extend(lifecycle.records().map(|row| row.reference.clone()));
                references.extend(lifecycle.absences().cloned());
                references.extend_from_slice(dependencies);
                references.sort_unstable_by(|left, right| {
                    (
                        left.owner_kind as u8,
                        left.owner_id.as_str(),
                        left.resource_kind as u8,
                        left.resource_id.as_str(),
                        left.revision_id.as_str(),
                    )
                        .cmp(&(
                            right.owner_kind as u8,
                            right.owner_id.as_str(),
                            right.resource_kind as u8,
                            right.resource_id.as_str(),
                            right.revision_id.as_str(),
                        ))
                });
                references.dedup();
                Ok(references)
            })
            .map_err(Failure::from)
    }

    pub(super) fn directory_failure(&self, error: anyhow::Error) -> Failure {
        if error.is::<directory::CapacityExceeded>() {
            ErrorCode::CapacityExceeded.into()
        } else {
            self.unavailable(ErrorCode::StorageUnavailable)
        }
    }
}
