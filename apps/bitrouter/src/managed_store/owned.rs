//! The owned native journal remains one scoped, versioned row.
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::core::protocol::{CoreError, OwnershipGrant};
use sea_orm::sea_query::{Alias, Expr, Func};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, QueryFilter,
    QuerySelect, QueryTrait, Set,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{session, storage};
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
}

struct Attempt {
    revision: u64,
    digest: [u8; 32],
}

pub(crate) struct Loaded {
    pub revision: u64,
    pub descriptor: Descriptor,
    pub journal: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
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
        })
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
        let digest = journal_digest(&loaded.journal)?;
        let attempted = self
            .attempted
            .lock()
            .map_err(|_| storage("native write evidence unavailable"))?;
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
            .reconcile_native_journal(binding.clone())
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
        let attempt = Attempt {
            revision: expected_revision
                .checked_add(1)
                .ok_or_else(|| storage("native revision exhausted"))?,
            digest: journal_digest(&bytes)?,
        };
        *self
            .attempted
            .lock()
            .map_err(|_| storage("native write evidence unavailable"))? = Some(attempt);
        self.last_write.store(1, Ordering::Release);
        match self
            .authority
            .service()
            .save_native_journal(
                binding.clone(),
                expected_revision,
                bytes,
                self.established.load(Ordering::Acquire),
            )
            .await
        {
            Ok(()) => {
                self.last_write.store(2, Ordering::Release);
                self.established.store(true, Ordering::Release);
                Ok(())
            }
            Err(failure) => {
                let error = failure.core();
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
fn journal_digest(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(storage("native journal exceeds storage allowance"));
    }
    let journal: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| storage("invalid native write evidence"))?;
    let canonical = encode(&journal).map_err(|_| storage("native write evidence exceeds bound"))?;
    Ok(Sha256::digest(canonical).into())
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
    check()?;
    let query = scoped(authority)?;
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
    let metadata = query
        .clone()
        .select_only()
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
        .expr_as(bytes.clone(), "actual_bytes")
        .build(tx.get_database_backend());
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
    let (scope, _) = Scope::for_session(authority);
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
        != Some(authority.descriptor().revision.as_str())
    {
        return Err(crate::tenancy::session::Rejected(
            bitrouter_ai::tenancy::operation::ErrorCode::RevisionConflict,
        )
        .into());
    }
    // Guard the actual cell too, even if a backend's isolation profile changes.
    let payload = query
        .select_only()
        .column(session::Column::Payload)
        .filter(session::Column::Revision.eq(revision))
        .filter(Expr::expr(bytes).lte(MAX_STORE_BYTES as i64))
        .into_tuple::<String>()
        .one(tx)
        .await?
        .context("native journal changed while reading")?;
    check()?;
    ensure!(
        payload.len() == actual as usize,
        "native journal byte length changed"
    );
    let envelope: Envelope = serde_json::from_str(&payload)?;
    ensure!(
        envelope.format == 1 && authority.matches(&envelope.descriptor),
        "native ownership mismatch"
    );
    authority
        .validate_descriptor(&envelope.descriptor)
        .map_err(|failure| crate::tenancy::session::Rejected(failure.0.code))?;
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
    }))
}

pub(crate) async fn save_in(
    tx: &DatabaseTransaction,
    binding: &Binding,
    expected: u64,
    journal: &[u8],
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    check()?;
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
        format: 1,
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
