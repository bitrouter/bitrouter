//! Revision-fenced directory pages; all durable reads run on the authority actor.

use axum::{
    Extension, Json,
    extract::{Query, State, rejection::QueryRejection},
};
use bitrouter_ai::tenancy::{
    directory::{
        DirectoryCursorV1, DirectoryEntryV1, DirectoryPageV1, MAX_DIRECTORY_PAGE_BYTES,
        ManagementAuthorityClassV1, ManagementOwnerV1,
    },
    identity::AuthorityRevision,
    operation::ErrorCode,
    validation::Validate,
};
use serde::Deserialize;
use tokio::{
    sync::{OwnedSemaphorePermit, oneshot},
    time::Instant,
};

use super::{
    MANAGEMENT_TIMEOUT, Service,
    actor::{Job, Worker},
    error::Failure,
    model::{AuthorityClass, Caller},
    store::{self, directory::PageBudget},
    transaction::Control,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageQuery {
    authority_revision: Option<u64>,
    after: Option<DirectoryCursorV1>,
    #[serde(default = "default_limit")]
    limit: u16,
}
fn default_limit() -> u16 {
    100
}

pub(super) struct Read {
    caller: Caller,
    revision: Option<AuthorityRevision>,
    after: Option<DirectoryCursorV1>,
    limit: u16,
    pub deadline: Instant,
    pub permit: OwnedSemaphorePermit,
    pub reply: oneshot::Sender<Result<DirectoryPageV1, Failure>>,
}

fn operator(caller: &Caller) -> Result<(), Failure> {
    if matches!(caller, Caller::Installation(_)) {
        Ok(())
    } else {
        Err(ErrorCode::Forbidden.into())
    }
}

pub(super) async fn http(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<DirectoryPageV1>, Failure> {
    operator(&caller)?;
    let Query(query) = query.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let revision = query
        .authority_revision
        .map(AuthorityRevision::try_from)
        .transpose()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    service
        .directory(&caller, revision, query.after, query.limit)
        .await
        .map(Json)
}

impl Service {
    pub(super) async fn directory(
        &self,
        caller: &Caller,
        revision: Option<AuthorityRevision>,
        after: Option<DirectoryCursorV1>,
        limit: u16,
    ) -> Result<DirectoryPageV1, Failure> {
        operator(caller)?;
        if !(1..=100).contains(&limit) || (after.is_some() && revision.is_none()) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let deadline = Instant::now() + MANAGEMENT_TIMEOUT;
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let (reply, receive) = oneshot::channel();
        self.enqueue(
            Job::Directory(Read {
                caller: caller.clone(),
                revision,
                after,
                limit,
                deadline,
                permit,
                reply,
            }),
            false,
        )?;
        let page = match tokio::time::timeout_at(deadline, receive).await {
            Ok(Ok(result)) => result?,
            _ => return Err(ErrorCode::AuthorityUnavailable.into()),
        };
        // Reject a page if another command published after the actor read it.
        // The next request independently authenticates control:tenancy again.
        if self.current()?.revision != page.authority_revision {
            return Err(ErrorCode::RevisionConflict.into());
        }
        Ok(page)
    }
}

impl Worker {
    pub(super) async fn directory(
        &self,
        read: &Read,
        control: &Control,
    ) -> Result<DirectoryPageV1, Failure> {
        operator(&read.caller)?;
        let state = self.current()?;
        if read
            .revision
            .is_some_and(|revision| revision != state.revision)
        {
            return Err(ErrorCode::RevisionConflict.into());
        }
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let mut limit = read.limit;
        let result = loop {
            let result = self
                .directory_entries(&tx, read, state.revision, limit, control)
                .await;
            match result {
                Err(error) if error.is::<store::directory::CapacityExceeded>() && limit > 1 => {
                    // The SQL projection divides its fixed byte allowance by
                    // rows, including a continuation sentinel. Retry only this
                    // bounded read in the same snapshot, before building a page.
                    // Halving reaches one returned row and one sentinel, then
                    // refuses; neither the allowance nor deadline increases.
                    limit = (limit / 2).max(1);
                }
                result => break result,
            }
        };
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let entries = result.map_err(|error| {
            if error.is::<store::directory::CapacityExceeded>() {
                Failure::from(ErrorCode::CapacityExceeded)
            } else if error.is::<super::transaction::ReadExpired>() {
                Failure::from(ErrorCode::AuthorityUnavailable)
            } else {
                self.unavailable(ErrorCode::StorageUnavailable)
            }
        })?;
        let available = entries.len();
        let mut page = DirectoryPageV1 {
            schema_version: 1,
            installation_id: state.installation.clone(),
            authority_revision: state.revision,
            entries: Vec::new(),
            next_after: None,
        };
        let mut selected = Vec::new();
        let mut entry_bytes = 0;
        // Measure each entry once, plus the empty envelope with its proposed
        // cursor. Array commas add exactly one byte per preceding entry. Keep
        // the first entry that does not fit for the next request, without ever
        // serializing an oversized response or repeatedly encoding its prefix.
        for (index, entry) in entries.into_iter().take(usize::from(limit)).enumerate() {
            let size = serde_json::to_vec(&entry)
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
                .len();
            page.next_after = (index + 1 < available).then(|| entry.cursor());
            let envelope = serde_json::to_vec(&page)
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            if envelope.len() + entry_bytes + size + index > MAX_DIRECTORY_PAGE_BYTES {
                page.next_after = selected.last().map(DirectoryEntryV1::cursor);
                if selected.is_empty() {
                    return Err(ErrorCode::PayloadTooLarge.into());
                }
                break;
            }
            entry_bytes += size;
            selected.push(entry);
        }
        page.entries = selected;
        page.validate()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        Ok(page)
    }

    async fn directory_entries(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        read: &Read,
        revision: AuthorityRevision,
        limit: u16,
        control: &Control,
    ) -> anyhow::Result<Vec<DirectoryEntryV1>> {
        use store::directory as facts;
        control.check_read()?;
        let header = store::startup::read_header_in(tx, 4096)
            .await?
            .ok_or_else(|| anyhow::anyhow!("directory authority missing"))?;
        anyhow::ensure!(
            header.header.revision == revision,
            "directory authority mismatch"
        );
        let bound = usize::from(limit) + 1;
        let after = read.after.as_ref().map(DirectoryCursorV1::key);
        let bytes = self.authority.load_budget() / 4;
        let mut entries = Vec::new();
        for kind in ["binding", "credential", "principal", "tenant"] {
            if entries.len() == bound {
                break;
            }
            if after.is_some_and(|key| key.0 > kind) {
                continue;
            }
            control.check_read()?;
            let budget = PageBudget {
                rows: bound - entries.len(),
                bytes,
            };
            let cursor = after.filter(|key| key.0 == kind);
            match kind {
                "binding" => {
                    let after = cursor
                        .map(|(_, tenant, id)| {
                            Ok::<_, anyhow::Error>((tenant.try_into()?, id.try_into()?))
                        })
                        .transpose()?;
                    entries.extend(
                        facts::binding_page_in(tx, after.as_ref(), budget)
                            .await?
                            .into_iter()
                            .map(|binding| DirectoryEntryV1::Binding {
                                tenant_id: binding.tenant,
                                binding_id: binding.id,
                                revision_id: binding.revision,
                                principal_id: binding.principal,
                                role: binding.role,
                                scope: binding.scope,
                                expires_at: binding.expires.wire,
                                management_owner: binding.managed_by.map(|owner| {
                                    ManagementOwnerV1 {
                                        principal_id: owner.principal,
                                        authority_class: match owner.class {
                                            AuthorityClass::Installation => {
                                                ManagementAuthorityClassV1::Installation
                                            }
                                            AuthorityClass::Tenant => {
                                                ManagementAuthorityClassV1::Tenant
                                            }
                                        },
                                    }
                                }),
                            }),
                    );
                }
                "credential" => {
                    let after = cursor.map(|(_, tenant, id)| (tenant, id));
                    entries.extend(
                        facts::pages::credential_directory_page_in(tx, after, budget)
                            .await?
                            .into_iter()
                            .map(|credential| DirectoryEntryV1::Credential {
                                tenant_id: credential.tenant,
                                credential_id: credential.id,
                                revision_id: credential.revision,
                                principal_id: credential.principal,
                                binding_id: credential.binding,
                                permissions: credential.permissions,
                                scope: credential.scope,
                                expires_at: credential.expires.wire,
                                recorded_state: credential.state,
                                audit_reader: credential.audit_reader,
                            }),
                    );
                }
                "principal" => {
                    let after = cursor.map(|(_, _, id)| id.try_into()).transpose()?;
                    entries.extend(
                        facts::principal_page_in(tx, after.as_ref(), budget)
                            .await?
                            .into_iter()
                            .map(|principal| DirectoryEntryV1::Principal {
                                principal_id: principal.id,
                                revision_id: principal.revision,
                                issuer: principal.issuer,
                                subject: principal.subject,
                            }),
                    );
                }
                "tenant" => {
                    let after = cursor.map(|(_, _, id)| id.try_into()).transpose()?;
                    entries.extend(
                        facts::tenant_page_in(tx, after.as_ref(), budget)
                            .await?
                            .into_iter()
                            .map(|tenant| DirectoryEntryV1::Tenant {
                                tenant_id: tenant.id,
                                revision_id: tenant.revision,
                                state: tenant.state,
                            }),
                    );
                }
                _ => anyhow::bail!("unsupported directory kind"),
            }
        }
        control.check_read()?;
        Ok(entries)
    }
}
