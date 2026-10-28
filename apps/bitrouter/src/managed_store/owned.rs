//! The owned native journal remains one scoped, versioned row.
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::core::protocol::{CoreError, OwnershipGrant};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, QueryFilter,
    QuerySelect, QueryTrait, Set,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{session, storage};
use crate::tenancy::session::recovery::{Budget, CheckedRecovery, Metadata, Phase};
use crate::{
    harness::resources::managed::store::{MAX_STORE_BYTES, NativeStore},
    storage::scope::{KeyDomain, MetadataRef, Scope},
    tenancy::execution::session::{Authority, Binding, Descriptor},
};

pub(crate) struct Store {
    authority: Arc<Authority>,
    descriptor: OnceLock<Descriptor>,
    binding: OnceLock<Arc<Binding>>,
    established: AtomicBool,
    last_write: AtomicU8,
    load_unconfirmed: AtomicBool,
    attempted: Mutex<Option<Attempt>>,
    metadata: Mutex<Option<Metadata>>,
    recovery: OnceLock<Arc<CheckedRecovery>>,
    qualified: AtomicBool,
}

struct Attempt {
    revision: u64,
    digest: [u8; 32],
}

#[derive(Clone)]
pub(crate) struct Loaded {
    pub revision: u64,
    pub descriptor: Descriptor,
    pub journal: Vec<u8>,
    pub metadata: Option<Metadata>,
    pub row_digest: [u8; 32],
    pub recovery: Option<Arc<CheckedRecovery>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery: Option<Metadata>,
    descriptor: Descriptor,
    journal: serde_json::Value,
}

impl Store {
    pub(crate) fn new(authority: Arc<Authority>) -> Arc<Self> {
        Arc::new(Self {
            authority,
            descriptor: OnceLock::new(),
            binding: OnceLock::new(),
            established: AtomicBool::new(false),
            last_write: AtomicU8::new(0),
            load_unconfirmed: AtomicBool::new(false),
            attempted: Mutex::new(None),
            metadata: Mutex::new(None),
            recovery: OnceLock::new(),
            qualified: AtomicBool::new(false),
        })
    }
    pub(crate) fn recovery(&self) -> Option<&Arc<CheckedRecovery>> {
        self.recovery.get()
    }
    pub(crate) async fn begin_epoch(
        &self,
        expected: u64,
        grant: &OwnershipGrant,
        workspace: &std::path::Path,
        mode: bitrouter_ai::harness::agent::ToolMode,
        previous: Option<&[u8]>,
        tool_free_config: bool,
    ) -> Result<u64, CoreError> {
        let origin = self.authority.startup_stamp().map_err(storage)?;
        let Some(origin) = origin else {
            return Ok(expected);
        };
        self.qualified.store(
            tool_free_config && self.authority.recovery_profile(),
            Ordering::Release,
        );
        *self
            .metadata
            .lock()
            .map_err(|_| storage("native metadata unavailable"))? = Some(Metadata {
            origin,
            phase: Phase::Opening,
            eligible: false,
            profile_digest: None,
            workspace_grant: None,
        });
        let previous = previous
            .map(serde_json::from_slice::<serde_json::Value>)
            .transpose()
            .map_err(|_| storage("invalid predecessor journal"))?;
        let marker = encode(
            &serde_json::json!({"grant":grant,"workspace":workspace,"mode":mode,
            "opening":true,"previous":previous}),
        )
        .map_err(|_| storage("opening journal bound"))?;
        self.save(expected, marker).await?;
        expected
            .checked_add(1)
            .ok_or_else(|| storage("native revision exhausted"))
    }
    pub(crate) fn retire_recovery(&self) -> Result<(), CoreError> {
        let checked = self
            .recovery
            .get()
            .ok_or_else(|| storage("recovery participant missing"))?;
        if !checked.committed() {
            return Err(storage("recovery commit is not confirmed"));
        }
        let mut metadata = self
            .metadata
            .lock()
            .map_err(|_| storage("native metadata unavailable"))?;
        let state = metadata
            .as_mut()
            .ok_or_else(|| storage("recovery metadata missing"))?;
        state.phase = Phase::Ready;
        state.workspace_grant = None;
        Ok(())
    }
    pub(crate) fn confirm_readback(&self) {
        self.established.store(true, Ordering::Release);
        self.last_write.store(2, Ordering::Release);
    }
    pub(crate) fn write_not_committed(&self) -> bool {
        self.last_write.load(Ordering::Acquire) == 0
    }
    pub(crate) fn load_unconfirmed(&self) -> bool {
        self.load_unconfirmed.load(Ordering::Acquire)
    }
    pub(crate) fn matches_attempt(&self, loaded: &Loaded) -> Result<bool, CoreError> {
        let (_, digest) = journal_evidence(&loaded.journal)?;
        let attempted = self
            .attempted
            .lock()
            .map_err(|_| storage("native write evidence unavailable"))?;
        if self.recovery.get().is_some()
            && loaded.metadata
                != *self
                    .metadata
                    .lock()
                    .map_err(|_| storage("native provenance unavailable"))?
        {
            return Ok(false);
        }
        Ok(attempted
            .as_ref()
            .is_some_and(|attempt| loaded.revision == attempt.revision && digest == attempt.digest))
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        self.descriptor.get().unwrap_or(self.authority.descriptor())
    }
    pub(crate) async fn reconcile(&self) -> Result<Loaded, CoreError> {
        let binding = self
            .binding
            .get()
            .ok_or_else(|| storage("native store unbound at readback"))?;
        let loaded = self
            .authority
            .service()
            .reconcile_native_journal(binding.clone(), self.recovery.get().cloned())
            .await
            .map_err(crate::tenancy::session::NativeFailure::core)?
            .ok_or_else(|| storage("native row missing at readback"))?;
        if &loaded.descriptor != self.descriptor() {
            return Err(storage("native readback descriptor changed"));
        }
        Ok(loaded)
    }
    pub(crate) fn attach(&self, binding: Arc<Binding>) -> Result<(), CoreError> {
        if binding.descriptor() != self.descriptor() {
            return Err(storage("native descriptor changed"));
        }
        self.binding
            .set(binding)
            .map_err(|_| storage("native store already bound"))
    }
}

#[async_trait]
impl NativeStore for Store {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError> {
        self.load_unconfirmed.store(true, Ordering::Release);
        let outcome = self
            .authority
            .service()
            .load_native_journal(self.authority.clone())
            .await;
        let loaded = match outcome {
            Ok(loaded) => {
                self.load_unconfirmed.store(false, Ordering::Release);
                loaded
            }
            Err(failure) => {
                let error = failure.core();
                if error.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted {
                    self.load_unconfirmed.store(false, Ordering::Release);
                }
                return Err(error);
            }
        };
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        *self
            .metadata
            .lock()
            .map_err(|_| storage("native metadata unavailable"))? = loaded.metadata.clone();
        if let Some(recovery) = loaded.recovery {
            self.recovery
                .set(recovery)
                .map_err(|_| storage("recovery already prepared"))?;
        }
        self.descriptor
            .set(loaded.descriptor)
            .map_err(|_| storage("native store was already loaded"))?;
        Ok(Some((loaded.revision, loaded.journal)))
    }
    async fn save(&self, expected_revision: u64, bytes: Vec<u8>) -> Result<(), CoreError> {
        let binding = self
            .binding
            .get()
            .ok_or_else(|| storage("native store is unbound"))?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| storage("native revision exhausted"))?;
        let (value, digest) = journal_evidence(&bytes)?;
        let attempt = Attempt { revision, digest };
        *self
            .attempted
            .lock()
            .map_err(|_| storage("native write evidence unavailable"))? = Some(attempt);
        self.last_write.store(1, Ordering::Release);
        let metadata = {
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| storage("native metadata unavailable"))?;
            if let Some(state) = metadata.as_mut() {
                if self.recovery.get().is_some_and(|r| !r.committed()) {
                    state.origin = self
                        .authority
                        .startup_stamp()
                        .map_err(storage)?
                        .ok_or_else(|| storage("recovery origin missing"))?;
                    state.phase = Phase::Retiring;
                    state.workspace_grant = self
                        .recovery
                        .get()
                        .map(|checked| checked.workspace_predecessor().clone());
                } else if state.phase == Phase::Opening
                    && value.get("manifest").is_some()
                    && value
                        .get("checkpoint")
                        .and_then(|batch| batch.get("identity"))
                        .is_some_and(|identity| {
                            identity.get("execution_epoch") == value["grant"].get("execution_epoch")
                                && identity.get("core_instance_id")
                                    == value["grant"].get("core_instance_id")
                                && identity.get("session_id") == value["grant"].get("session_id")
                        })
                {
                    state.phase = Phase::Ready;
                    state.eligible = self.qualified.load(Ordering::Acquire);
                    state.profile_digest = if state.eligible {
                        Some(self.authority.recovery_profile_digest().map_err(storage)?)
                    } else {
                        None
                    };
                }
                if value
                    .get("starts")
                    .and_then(|v| v.as_object())
                    .is_some_and(|v| !v.is_empty())
                {
                    state.eligible = false;
                    state.profile_digest = None;
                }
                if value.get("released").and_then(|v| v.as_bool()) == Some(true) {
                    state.phase = Phase::Released;
                }
            }
            metadata.clone()
        };
        #[cfg(test)]
        let started = tokio::time::Instant::now();
        match self
            .authority
            .service()
            .save_native_journal(
                binding.clone(),
                expected_revision,
                bytes,
                self.established.load(Ordering::Acquire),
                metadata,
                self.recovery.get().filter(|r| !r.committed()).cloned(),
            )
            .await
        {
            Ok(()) => {
                // Test scheduling only: the actual writer has already published
                // its durable marker and accounting; Core has not received ACK.
                #[cfg(all(test, unix, feature = "harness"))]
                if self
                    .recovery
                    .get()
                    .is_some_and(|recovery| recovery.committed())
                    && let Some(gate) = self
                        .authority
                        .service()
                        .take_host_gate(crate::tenancy::hosts::HostBoundary::RecoveryCommitted)?
                {
                    gate.reached().await;
                }
                self.last_write.store(2, Ordering::Release);
                self.established.store(true, Ordering::Release);
                Ok(())
            }
            Err(failure) => {
                let error = failure.core();
                #[cfg(test)]
                eprintln!(
                    "native journal save failed: code={:?}, commit={:?}, elapsed_ms={}",
                    error.code,
                    error.commit_status,
                    started.elapsed().as_millis()
                );
                if error.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted {
                    self.last_write.store(0, Ordering::Release);
                }
                Err(error)
            }
        }
    }
}

// Only a digest of the exact bounded attempted journal is retained. Reconciliation
// cannot substitute a matching checkpoint with different artifacts or tool starts.
// Return the decoded value so save can also inspect metadata without reparsing.
fn journal_evidence(bytes: &[u8]) -> Result<(serde_json::Value, [u8; 32]), CoreError> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(storage("native journal exceeds storage allowance"));
    }
    let journal: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| storage("invalid native write evidence"))?;
    let canonical = encode(&journal).map_err(|_| storage("native write evidence exceeds bound"))?;
    Ok((journal, Sha256::digest(canonical).into()))
}

fn scoped(authority: &Authority) -> Result<sea_orm::Select<session::Entity>> {
    let (scope, _) = Scope::for_session(authority);
    let descriptor = authority.descriptor();
    let key = scope.physical_key(
        KeyDomain::NativeSession,
        "",
        &[descriptor.principal.as_str(), &descriptor.name],
    )?;
    Ok(session::Entity::find_by_id(key)
        .filter(scope.predicate(session::Column::InstallationId, session::Column::TenantId))
        .filter(session::Column::SessionPrincipalId.eq(descriptor.principal.as_str()))
        .filter(session::Column::LogicalName.eq(&descriptor.name)))
}

pub(crate) async fn load_in(
    tx: &DatabaseTransaction,
    authority: &Authority,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<Option<Loaded>> {
    load_bounded_in(tx, authority, check, None).await
}

#[derive(Clone, Copy)]
enum ReadBudget<'a, 'b> {
    Recovery(&'a Budget),
    Operation(&'a crate::storage::operation_budget::OperationBudget<'b>),
}
enum ReadSource<'a> {
    Review(&'a Binding, &'a str),
    PaidRecovery(
        &'a crate::tenancy::learning::recovery::RecoveryIdentity,
        &'a Descriptor,
    ),
    Session(&'a Authority),
    Learner(&'a crate::tenancy::learner::identity::ReadIdentity, &'a str),
}
impl ReadSource<'_> {
    fn scope(&self) -> Scope {
        match self {
            Self::Session(authority) => Scope::for_session(authority).0,
            Self::PaidRecovery(identity, _) => Scope::for_paid_recovery(identity).0,
            Self::Review(binding, _) => Scope::for_session(binding.authority()).0,
            Self::Learner(identity, _) => Scope::for_learner(identity).0,
        }
    }
    fn revision(&self) -> &str {
        match self {
            Self::Session(authority) => authority.descriptor().revision.as_str(),
            Self::PaidRecovery(_, descriptor) => descriptor.revision.as_str(),
            Self::Review(binding, _) => binding.descriptor().revision.as_str(),
            Self::Learner(identity, _) => identity.source_revision().as_str(),
        }
    }
    fn query(&self) -> Result<sea_orm::Select<session::Entity>> {
        match self {
            Self::Session(authority) => scoped(authority),
            Self::PaidRecovery(_, descriptor) => {
                Ok(session::Entity::find_by_id(self.scope().physical_key(
                    KeyDomain::NativeSession,
                    "",
                    &[descriptor.principal.as_str(), &descriptor.name],
                )?)
                .filter(
                    self.scope()
                        .predicate(session::Column::InstallationId, session::Column::TenantId),
                ))
            }
            Self::Review(binding, key) => {
                crate::tenancy::experiment::queue::check_reader(binding)
                    .map_err(crate::storage::operation_budget::Error::Rejected)?;
                Ok(session::Entity::find_by_id(*key).filter(
                    self.scope()
                        .predicate(session::Column::InstallationId, session::Column::TenantId),
                ))
            }
            Self::Learner(identity, key) => {
                identity
                    .check(&[])
                    .map_err(crate::storage::operation_budget::Error::Rejected)?;
                Ok(session::Entity::find_by_id(*key).filter(
                    self.scope()
                        .predicate(session::Column::InstallationId, session::Column::TenantId),
                ))
            }
        }
    }
    fn validate(&self, descriptor: &Descriptor) -> Result<()> {
        match self {
            Self::PaidRecovery(identity, expected) => {
                let (installation, tenant, principal) = identity.storage_identity();
                ensure!(
                    descriptor == *expected
                        && &descriptor.installation == installation
                        && &descriptor.tenant == tenant
                        && &descriptor.principal == principal,
                    "paid stopped native namespace changed"
                );
            }
            Self::Review(binding, key) => {
                crate::tenancy::experiment::queue::check_reader(binding)
                    .map_err(crate::storage::operation_budget::Error::Rejected)?;
                let (installation, tenant, _) = binding.authority().storage_identity();
                ensure!(
                    &descriptor.installation == installation
                        && &descriptor.tenant == tenant
                        && descriptor.revision == binding.descriptor().revision
                        && self.scope().physical_key(
                            KeyDomain::NativeSession,
                            "",
                            &[descriptor.principal.as_str(), &descriptor.name]
                        )? == *key,
                    "native review source namespace mismatch"
                );
            }
            Self::Session(authority) => {
                ensure!(authority.matches(descriptor), "native ownership mismatch");
                authority
                    .validate_descriptor(descriptor)
                    .map_err(|failure| crate::tenancy::session::Rejected(failure.0.code))?;
            }
            Self::Learner(identity, key) => {
                identity
                    .check(&[])
                    .map_err(crate::storage::operation_budget::Error::Rejected)?;
                let (installation, tenant, _) = identity.storage_identity();
                ensure!(
                    &descriptor.installation == installation
                        && &descriptor.tenant == tenant
                        && &descriptor.revision == identity.source_revision()
                        && self.scope().physical_key(
                            KeyDomain::NativeSession,
                            &descriptor.name,
                            &[descriptor.principal.as_str(), &descriptor.name]
                        )? == *key,
                    "learner native source namespace mismatch"
                );
            }
        }
        Ok(())
    }
}

pub(crate) async fn load_bounded_in(
    tx: &DatabaseTransaction,
    authority: &Authority,
    check: &(dyn Fn() -> Result<()> + Sync),
    budget: Option<&Budget>,
) -> Result<Option<Loaded>> {
    load_source_in(
        tx,
        ReadSource::Session(authority),
        check,
        budget.map(ReadBudget::Recovery),
    )
    .await
}
pub(crate) async fn load_for_model_in(
    tx: &DatabaseTransaction,
    authority: &Authority,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Loaded>> {
    let check = || budget.check().map_err(anyhow::Error::from);
    load_source_in(
        tx,
        ReadSource::Session(authority),
        &check,
        Some(ReadBudget::Operation(budget)),
    )
    .await
}
pub(crate) async fn load_for_paid_recovery_in(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    descriptor: &Descriptor,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Loaded>> {
    let check = || budget.check().map_err(anyhow::Error::from);
    load_source_in(
        tx,
        ReadSource::PaidRecovery(identity, descriptor),
        &check,
        Some(ReadBudget::Operation(budget)),
    )
    .await
}

pub(crate) async fn load_for_learner_in(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learner::identity::ReadIdentity,
    key: &str,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Loaded>> {
    let check = || {
        identity
            .check(&[])
            .map_err(crate::storage::operation_budget::Error::Rejected)?;
        budget.check()?;
        Ok(())
    };
    load_source_in(
        tx,
        ReadSource::Learner(identity, key),
        &check,
        Some(ReadBudget::Operation(budget)),
    )
    .await
}

/// A tenant-wide checked read does not bind, reopen or adopt the source owner.
pub(crate) async fn load_for_review_in(
    tx: &DatabaseTransaction,
    binding: &Binding,
    key: &str,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<Option<Loaded>> {
    let check = || {
        crate::tenancy::experiment::queue::check_reader(binding)
            .map_err(crate::storage::operation_budget::Error::Rejected)?;
        budget.check()?;
        Ok(())
    };
    load_source_in(
        tx,
        ReadSource::Review(binding, key),
        &check,
        Some(ReadBudget::Operation(budget)),
    )
    .await
}

async fn load_source_in(
    tx: &DatabaseTransaction,
    source: ReadSource<'_>,
    check: &(dyn Fn() -> Result<()> + Sync),
    budget: Option<ReadBudget<'_, '_>>,
) -> Result<Option<Loaded>> {
    check()?;
    // A current owner lock precedes the counter lock and every consistent read.
    // It does not authorize dispatch or turn an unconfirmed owner into a release.
    let query = if budget.is_some() {
        let mut query = source.query()?.lock_exclusive();
        for name in [
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "session_principal_id",
            "logical_name",
            "harness_id",
            "session_id",
            "execution_revision",
        ] {
            let length = match tx.get_database_backend() {
                DatabaseBackend::Sqlite => format!("length(CAST({name} AS BLOB))"),
                _ => format!("OCTET_LENGTH({name})"),
            };
            query = query.filter(Expr::cust(length).lte(512_i64));
        }
        query
    } else {
        source.query()?
    };
    let bytes = match tx.get_database_backend() {
        DatabaseBackend::Sqlite => Expr::cust("length(CAST(payload AS BLOB))"),
        // PostgreSQL returns integer (INT4); the bounded metadata reader uses i64.
        // https://www.postgresql.org/docs/15/functions-string.html
        DatabaseBackend::Postgres => Func::cast_as(
            Func::cust(Alias::new("OCTET_LENGTH")).arg(Expr::col(session::Column::Payload)),
            Alias::new("BIGINT"),
        )
        .into(),
        DatabaseBackend::MySql => Func::cust(Alias::new("OCTET_LENGTH"))
            .arg(Expr::col(session::Column::Payload))
            .into(),
    };
    let mut projection = query.clone().select_only();
    if let Some(ReadBudget::Recovery(budget)) = budget {
        ensure!(
            tx.get_database_backend() == DatabaseBackend::Sqlite,
            "recovery metadata requires file SQLite"
        );
        budget.charge(0, 2048)?;
        for column in ["revision", "payload_bytes"] {
            projection = projection.expr_as(
                Expr::cust(format!(
                    "CASE WHEN typeof({column})='integer' THEN {column} ELSE NULL END"
                )),
                column,
            );
        }
        for column in [
            "installation_id",
            "tenant_id",
            "creator_principal_id",
            "session_principal_id",
            "logical_name",
            "harness_id",
            "session_id",
            "execution_revision",
        ] {
            projection=projection.expr_as(Expr::cust(format!(
                "CASE WHEN typeof({column})='text' AND length(CAST({column} AS BLOB))<=128 THEN {column} ELSE NULL END")),column);
        }
        projection = projection.expr_as(
            Expr::cust(
                "CASE WHEN typeof(payload)='text' THEN length(CAST(payload AS BLOB)) ELSE NULL END",
            ),
            "actual_bytes",
        );
    } else {
        projection = projection
            .columns([
                session::Column::Revision,
                session::Column::PayloadBytes,
                session::Column::InstallationId,
                session::Column::TenantId,
                session::Column::CreatorPrincipalId,
                session::Column::SessionPrincipalId,
                session::Column::LogicalName,
                session::Column::HarnessId,
                session::Column::SessionId,
                session::Column::ExecutionRevision,
            ])
            .expr_as(bytes.clone(), "actual_bytes");
    }
    let metadata = projection.build(tx.get_database_backend());
    let Some(metadata) = tx.query_one(metadata).await? else {
        check()?;
        return Ok(None);
    };
    check()?;
    let revision: i64 = metadata.try_get("", "revision")?;
    let declared: i64 = metadata.try_get("", "payload_bytes")?;
    let actual: i64 = metadata.try_get("", "actual_bytes")?;
    ensure!(
        revision > 0
            && actual >= 0
            && actual == declared
            && actual as u64 <= MAX_STORE_BYTES as u64,
        "invalid native journal length or revision"
    );
    let scope = source.scope();
    let installation: Option<String> = metadata.try_get("", "installation_id")?;
    let tenant: Option<String> = metadata.try_get("", "tenant_id")?;
    let creator: Option<String> = metadata.try_get("", "creator_principal_id")?;
    scope.validate_metadata(MetadataRef {
        installation: installation.as_deref(),
        tenant: tenant.as_deref(),
        creator: creator.as_deref(),
    })?;
    if metadata
        .try_get::<Option<String>>("", "execution_revision")?
        .as_deref()
        != Some(source.revision())
    {
        let code = bitrouter_ai::tenancy::operation::ErrorCode::RevisionConflict;
        return Err(match &source {
            ReadSource::Session(_) => crate::tenancy::session::Rejected(code).into(),
            ReadSource::Learner(_, _)
            | ReadSource::Review(_, _)
            | ReadSource::PaidRecovery(_, _) => {
                crate::storage::operation_budget::Error::Rejected(code).into()
            }
        });
    }
    if let Some(budget) = budget {
        let bytes = usize::try_from(actual)?;
        match budget {
            ReadBudget::Recovery(budget) => budget.charge(
                1,
                bytes
                    .checked_mul(3)
                    .context("native recovery allocation overflow")?,
            )?,
            ReadBudget::Operation(budget) => budget.charge(
                1,
                bytes
                    .checked_mul(5)
                    .and_then(|n| n.checked_add(8192))
                    .context("native source allocation overflow")?,
            )?,
        }
    }
    // Guard the actual cell too, even if a backend's isolation profile changes.
    let payload = query
        .select_only()
        .column(session::Column::Payload)
        .filter(session::Column::Revision.eq(revision))
        .filter(Expr::expr(bytes).eq(actual))
        .into_tuple::<String>()
        .one(tx)
        .await?
        .context("native journal changed while reading")?;
    check()?;
    ensure!(
        payload.len() == actual as usize,
        "native journal byte length changed"
    );
    let row_digest = Sha256::digest(payload.as_bytes()).into();
    let envelope: Envelope = serde_json::from_str(&payload)?;
    ensure!(
        ((envelope.format == 1 && envelope.recovery.is_none())
            || (envelope.format == 2 && envelope.recovery.is_some())),
        "native ownership mismatch"
    );
    if let Some(metadata) = &envelope.recovery {
        metadata.validate()?;
    }
    source.validate(&envelope.descriptor)?;
    for (column, expected) in [
        (
            "session_principal_id",
            envelope.descriptor.principal.as_str(),
        ),
        ("logical_name", envelope.descriptor.name.as_str()),
        ("harness_id", envelope.descriptor.harness.as_str()),
        ("session_id", envelope.descriptor.session.as_str()),
        ("execution_revision", envelope.descriptor.revision.as_str()),
    ] {
        ensure!(
            metadata.try_get::<Option<String>>("", column)?.as_deref() == Some(expected),
            "native metadata differs from journal"
        );
    }
    let grant: OwnershipGrant = serde_json::from_value(
        envelope
            .journal
            .get("grant")
            .context("native journal has no grant")?
            .clone(),
    )?;
    grant
        .validate()
        .map_err(|_| anyhow::anyhow!("native grant is invalid"))?;
    ensure!(
        grant.session_id == envelope.descriptor.session
            && grant.harness_id == envelope.descriptor.harness,
        "native grant differs from owner"
    );
    let journal = encode(&envelope.journal)?;
    check()?;
    Ok(Some(Loaded {
        revision: revision as u64,
        descriptor: envelope.descriptor,
        journal,
        metadata: envelope.recovery,
        row_digest,
        recovery: None,
    }))
}

pub(crate) async fn save_in(
    tx: &DatabaseTransaction,
    binding: &Binding,
    expected: u64,
    journal: &[u8],
    metadata: Option<&Metadata>,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    save_with_budget(tx, binding, expected, journal, metadata, check, None).await
}

pub(crate) async fn save_experiment_in(
    tx: &DatabaseTransaction,
    binding: &Binding,
    expected: u64,
    journal: &[u8],
    metadata: Option<&Metadata>,
    budget: &crate::storage::operation_budget::OperationBudget<'_>,
) -> Result<()> {
    let check = || budget.check().map_err(anyhow::Error::from);
    save_with_budget(
        tx,
        binding,
        expected,
        journal,
        metadata,
        &check,
        Some(budget),
    )
    .await
}

async fn save_with_budget(
    tx: &DatabaseTransaction,
    binding: &Binding,
    expected: u64,
    journal: &[u8],
    metadata: Option<&Metadata>,
    check: &(dyn Fn() -> Result<()> + Sync),
    budget: Option<&crate::storage::operation_budget::OperationBudget<'_>>,
) -> Result<()> {
    check()?;
    if let Some(budget) = budget {
        budget.charge(
            2,
            journal
                .len()
                .checked_mul(3)
                .and_then(|n| n.checked_add(8192))
                .ok_or(crate::storage::operation_budget::Error::Capacity)?,
        )?;
    }
    ensure!(
        journal.len() <= MAX_STORE_BYTES,
        "native journal exceeds storage allowance"
    );
    let journal: serde_json::Value = serde_json::from_slice(journal)?;
    let grant: OwnershipGrant = serde_json::from_value(
        journal
            .get("grant")
            .context("native journal has no grant")?
            .clone(),
    )?;
    binding
        .grant(&grant)
        .map_err(|_| anyhow::anyhow!("native journal grant mismatch"))?;
    let envelope = Envelope {
        format: if metadata.is_some() { 2 } else { 1 },
        recovery: metadata.cloned(),
        descriptor: binding.descriptor().clone(),
        journal,
    };
    let payload = String::from_utf8(encode(&envelope)?)?;
    check()?;
    let previous = i64::try_from(expected)?;
    let next = previous
        .checked_add(1)
        .context("native revision exhausted")?;
    let authority = binding.authority();
    if expected == 0 {
        let (scope, provenance) = Scope::for_session(authority);
        let metadata = scope.metadata(&provenance)?;
        let descriptor = binding.descriptor();
        session::Entity::insert(session::ActiveModel {
            id: Set(scope.physical_key(
                KeyDomain::NativeSession,
                "",
                &[descriptor.principal.as_str(), &descriptor.name],
            )?),
            revision: Set(next),
            payload_bytes: Set(payload.len() as i64),
            payload: Set(payload),
            installation_id: Set(metadata.installation),
            tenant_id: Set(metadata.tenant),
            creator_principal_id: Set(metadata.creator),
            session_principal_id: Set(Some(descriptor.principal.to_string())),
            logical_name: Set(Some(descriptor.name.clone())),
            harness_id: Set(Some(descriptor.harness.clone())),
            session_id: Set(Some(descriptor.session.clone())),
            execution_revision: Set(Some(descriptor.revision.to_string())),
        })
        .exec(tx)
        .await?;
    } else {
        let descriptor = binding.descriptor();
        let (scope, _) = Scope::for_session(authority);
        let key = scope.physical_key(
            KeyDomain::NativeSession,
            "",
            &[descriptor.principal.as_str(), &descriptor.name],
        )?;
        let changed = session::Entity::update_many()
            .col_expr(session::Column::Revision, Expr::value(next))
            .col_expr(
                session::Column::PayloadBytes,
                Expr::value(payload.len() as i64),
            )
            .col_expr(session::Column::Payload, Expr::value(payload))
            .filter(session::Column::Id.eq(key))
            .filter(scope.predicate(session::Column::InstallationId, session::Column::TenantId))
            .filter(session::Column::SessionPrincipalId.eq(descriptor.principal.as_str()))
            .filter(session::Column::LogicalName.eq(&descriptor.name))
            .filter(session::Column::HarnessId.eq(&descriptor.harness))
            .filter(session::Column::SessionId.eq(&descriptor.session))
            .filter(session::Column::ExecutionRevision.eq(descriptor.revision.as_str()))
            .filter(session::Column::Revision.eq(previous))
            .exec(tx)
            .await?;
        ensure!(changed.rows_affected == 1, "native journal CAS conflict");
    }
    check()
}

pub(crate) fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_STORE_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("native journal storage bound"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, value)?;
    Ok(output.0)
}

/// Canonical V2 starts with bounded provenance, before its potentially large
/// journal. This prefix only selects a decoder budget; full decoding and exact
/// row validation remain mandatory in the same read transaction.
pub(crate) async fn recovery_header(
    tx: &DatabaseTransaction,
    authority: &Authority,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<Option<Metadata>> {
    check()?;
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Sqlite,
        "recovery requires SQLite"
    );
    let header = scoped(authority)?
        .select_only()
        .expr_as(Expr::cust("substr(CAST(payload AS BLOB),1,8192)"), "header")
        .into_tuple::<Vec<u8>>()
        .one(tx)
        .await?;
    check()?;
    let Some(header) = header else {
        return Ok(None);
    };
    decode_recovery_header(&header)
}

fn decode_recovery_header(header: &[u8]) -> Result<Option<Metadata>> {
    ensure!(header.len() <= 8192, "native header bound");
    if header.starts_with(b"{\"format\":1,") {
        return Ok(None);
    }
    let remainder = header
        .strip_prefix(b"{\"format\":2,\"recovery\":")
        .context("native canonical format header")?;
    // Deserialize the complete metadata value, not an early-return object
    // visitor: serde_json requires that visitor to consume its closing brace.
    // Keep the prefix as bytes; truncation may split UTF-8 in the later journal.
    let mut input = serde_json::Deserializer::from_slice(remainder).into_iter::<Metadata>();
    let metadata = input.next().context("native recovery header missing")??;
    ensure!(
        remainder[input.byte_offset()..].starts_with(b",\"descriptor\":"),
        "native recovery header is not canonical"
    );
    metadata.validate()?;
    Ok(Some(metadata))
}

/// A fresh file claim excludes the previous process, but it does not prove that
/// any independently spawned native/capture effect was reaped. This first
/// profile therefore requires every other native row released and no capture
/// connection history in this installation.
pub(crate) async fn recovery_exclusion(
    tx: &DatabaseTransaction,
    authority: &Authority,
    tenants: &[bitrouter_ai::tenancy::identity::TenantId],
    budget: &Budget,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    use sea_orm::Statement;
    let installation = authority.descriptor().installation.as_str();
    ensure!(
        !tenants.is_empty()
            && tenants.len() <= 32
            && tenants.contains(&authority.descriptor().tenant),
        "native owner inventory unavailable"
    );
    check()?;
    let capture = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM acp_capture_connections WHERE installation_id=? LIMIT 1",
            [installation.into()],
        ))
        .await?;
    check()?;
    if capture.is_some() {
        return Err(crate::tenancy::session::Rejected(
            bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch,
        )
        .into());
    }
    let placeholders = vec!["?"; tenants.len()].join(",");
    let mut parameters: Vec<sea_orm::Value> = vec![installation.into()];
    parameters.extend(tenants.iter().map(|tenant| tenant.as_str().into()));
    let foreign=tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        format!("SELECT 1 FROM bro_managed_sessions WHERE installation_id=? AND (tenant_id IS NULL OR tenant_id NOT IN ({placeholders})) LIMIT 1"),parameters)).await?;
    check()?;
    ensure!(
        foreign.is_none(),
        "native owner has unknown tenant metadata"
    );
    let (scope, _) = Scope::for_session(authority);
    let own = scope.physical_key(
        KeyDomain::NativeSession,
        "",
        &[
            authority.descriptor().principal.as_str(),
            &authority.descriptor().name,
        ],
    )?;
    let text_columns = [
        "id",
        "creator_principal_id",
        "session_principal_id",
        "logical_name",
        "harness_id",
        "session_id",
        "execution_revision",
    ];
    let projection=text_columns.iter().map(|column|format!(
        "CASE WHEN typeof({column})='text' AND length(CAST({column} AS BLOB))<=128 THEN {column} ELSE NULL END AS {column}"))
        .collect::<Vec<_>>().join(",");
    for tenant in tenants {
        check()?;
        let rows=tx.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            format!("SELECT {projection}, CASE WHEN typeof(revision)='integer' THEN revision ELSE NULL END AS revision, CASE WHEN typeof(payload_bytes)='integer' THEN payload_bytes ELSE NULL END AS payload_bytes, CASE WHEN typeof(payload)='text' THEN length(CAST(payload AS BLOB)) ELSE NULL END AS actual FROM bro_managed_sessions WHERE installation_id=? AND tenant_id=? AND id<>? ORDER BY id LIMIT 4097"),
            [installation.into(),tenant.as_str().into(),own.clone().into()])).await?;
        check()?;
        budget.charge(
            rows.len(),
            rows.len()
                .checked_mul(1024)
                .context("native owner inventory bound")?,
        )?;
        for row in rows {
            check()?;
            let id: String = row.try_get("", "id")?;
            let declared: i64 = row.try_get("", "payload_bytes")?;
            let actual: i64 = row.try_get("", "actual")?;
            let revision: i64 = row.try_get("", "revision")?;
            ensure!(
                id.len() == 64
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                    && revision > 0
                    && actual >= 0
                    && declared == actual,
                "native owner metadata invalid"
            );
            budget.charge(
                0,
                usize::try_from(actual)?
                    .checked_mul(3)
                    .context("native owner byte bound")?,
            )?;
            let payload=tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions WHERE installation_id=? AND tenant_id=? AND id=? AND revision=? AND typeof(payload)='text' AND length(CAST(payload AS BLOB))=?",
                [installation.into(),tenant.as_str().into(),id.into(),revision.into(),actual.into()])).await?.context("native owner inventory changed")?;
            check()?;
            let bytes: String = payload.try_get("", "payload")?;
            ensure!(
                bytes.len() == usize::try_from(actual)?,
                "native owner payload length changed"
            );
            let envelope: Envelope = serde_json::from_str(&bytes)?;
            ensure!(
                envelope.descriptor.installation.as_str() == installation
                    && envelope.descriptor.tenant == *tenant
                    && ((envelope.format == 1 && envelope.recovery.is_none())
                        || (envelope.format == 2 && envelope.recovery.is_some())),
                "native peer ownership or format mismatch"
            );
            if let Some(metadata) = &envelope.recovery {
                metadata.validate()?;
            }
            if envelope.journal.get("released").and_then(|v| v.as_bool()) != Some(true)
                || envelope
                    .recovery
                    .as_ref()
                    .is_some_and(|m| m.phase != Phase::Released)
            {
                return Err(crate::tenancy::session::Rejected(
                    bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch,
                )
                .into());
            }
            for (column, expected) in [
                (
                    "creator_principal_id",
                    envelope.descriptor.principal.as_str(),
                ),
                (
                    "session_principal_id",
                    envelope.descriptor.principal.as_str(),
                ),
                ("logical_name", envelope.descriptor.name.as_str()),
                ("harness_id", envelope.descriptor.harness.as_str()),
                ("session_id", envelope.descriptor.session.as_str()),
                ("execution_revision", envelope.descriptor.revision.as_str()),
            ] {
                ensure!(
                    row.try_get::<String>("", column)? == expected,
                    "native owner metadata differs from payload"
                );
            }
            let journal = encode(&envelope.journal)?;
            crate::harness::resources::managed::store::recovery::validate_released_owner(
                &journal,
                &envelope.descriptor,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod recovery_header_tests {
    use super::*;
    #[test]
    fn canonical_recovery_header_stops_before_split_utf8_journal() -> Result<()> {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "origin":{"acquisition":"ee0ee4eb-3f2c-4ca7-bd2d-c41d8284a187","instance":"instance"},
            "phase":"ready","eligible":true,"profile_digest":"a".repeat(64)
        }))?;
        let mut bytes = format!(
            "{{\"format\":2,\"recovery\":{},\"descriptor\":{{}},\"journal\":\"",
            serde_json::to_string(&metadata)?
        )
        .into_bytes();
        bytes.resize(8191, b'x');
        bytes.push(0xe7); // First byte of a three-byte UTF-8 scalar, outside metadata.
        ensure!(std::str::from_utf8(&bytes).is_err());
        ensure!(decode_recovery_header(&bytes)? == Some(metadata));
        ensure!(
            decode_recovery_header(b"{\"format\":1,\"descriptor\":").is_ok_and(|v| v.is_none())
        );
        ensure!(decode_recovery_header(b"{\"format\":2,\"recovery\":{}").is_err());
        ensure!(decode_recovery_header(b"{\"format\":3,").is_err());
        Ok(())
    }
}
