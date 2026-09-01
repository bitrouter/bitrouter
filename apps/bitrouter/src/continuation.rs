//! Encrypted provider continuation registry and pipeline integration.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use bitrouter_sdk::error::{BitrouterError, Result as PipelineResult};
use bitrouter_sdk::language_model::auth::ContinuationAuthority;
use bitrouter_sdk::language_model::context::{
    PipelineContext, ProviderContinuation, RequireContinuationAuthority,
    SuppressProviderContinuation,
};
use bitrouter_sdk::language_model::hooks::{HookDecision, PreRequestHook, RouteHook};
use bitrouter_sdk::language_model::protocol::responses::{
    AssistantTurnCommitment, CausalPrefixCommitment, CausalPrefixPlan,
    decode_gateway_continuation_id, encode_gateway_continuation_id,
};
use bitrouter_sdk::language_model::settlement::{
    RequiredDeliveryHandshake, RequiredFinalizationContext, RequiredFinalizationReceipt,
    RequiredFinalizer,
};
use bitrouter_sdk::language_model::{ApiProtocol, AuthAppliers, Role, RoutingTarget};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, DatabaseConnection, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::workflow_state::predictive::authenticated_visible_causal_prefix;

const OWNER_IDENTITY_DOMAIN: &[u8] = b"bitrouter.continuation.owner.v1";
const CONTINUATION_IDENTITY_DOMAIN: &[u8] = b"bitrouter.continuation.identity.v1";
const TARGET_FINGERPRINT_DOMAIN: &[u8] = b"bitrouter.continuation.target.v1";
const KEY_ID_DOMAIN: &[u8] = b"bitrouter.continuation.key-id.v1";
const AEAD_AAD_DOMAIN: &[u8] = b"bitrouter.continuation.aead.v1";
const CIPHER_VERSION: i32 = 1;
const NONCE_BYTES: usize = 12;
const PUBLICATION_GENERATION_BYTES: usize = 16;
const PUBLICATION_INSTANCE_BYTES: usize = 16;
const PUBLICATION_LEASE_SECONDS: i64 = 30;
const PUBLICATION_RECONCILIATION_LEASE_SECONDS: i64 = 1;
const MAX_PENDING_PUBLICATIONS: usize = 256;
const RECONCILIATION_BATCH_SIZE: usize = 32;
const RECONCILIATION_BASE_DELAY: Duration = Duration::from_millis(25);
const RECONCILIATION_MAX_DELAY: Duration = Duration::from_secs(5);
const RECONCILIATION_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const RECONCILIATION_DRAIN_BATCH_TIMEOUT: Duration = Duration::from_secs(30);
const PUBLICATION_PROVISIONAL: &str = "provisional";
const PUBLICATION_DELIVERING: &str = "delivering";
const PUBLICATION_ACTIVE: &str = "active";
const CONTINUATION_PAYLOAD_VERSION: u8 = 4;
#[cfg(test)]
const CONTINUATION_PAYLOAD_V1_PREFIX: &str = "bitrouter-continuation-payload-v1:";
const CONTINUATION_PAYLOAD_V2_MAGIC: &[u8] = b"\xffbitrouter-continuation-payload-v2\0";
const CONTINUATION_PAYLOAD_V3_MAGIC: &[u8] = b"\xffbitrouter-continuation-payload-v3\0";
const CONTINUATION_PAYLOAD_V4_MAGIC: &[u8] = b"\xffbitrouter-continuation-payload-v4\0";
const MAX_EFFECTIVE_MODEL_BYTES: usize = 512;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ContinuationPayload {
    version: u8,
    provider_response_id: String,
    effective_model: Option<String>,
    #[serde(default)]
    effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    #[serde(default)]
    assistant_turn_commitment: Option<String>,
    #[serde(default)]
    causal_prefix_commitment: Option<String>,
}

impl ContinuationPayload {
    fn current(
        provider_response_id: &str,
        effective_model: &str,
        effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
        causal_prefix_commitment: Option<&CausalPrefixCommitment>,
    ) -> Result<Self> {
        validate_effective_model(effective_model)?;
        Ok(Self {
            version: CONTINUATION_PAYLOAD_VERSION,
            provider_response_id: provider_response_id.to_owned(),
            effective_model: Some(effective_model.to_owned()),
            effective_effort,
            assistant_turn_commitment: None,
            causal_prefix_commitment: causal_prefix_commitment
                .map(CausalPrefixCommitment::as_str)
                .map(ToOwned::to_owned),
        })
    }

    fn legacy(provider_response_id: String) -> Self {
        Self {
            version: 0,
            provider_response_id,
            effective_model: None,
            effective_effort: None,
            assistant_turn_commitment: None,
            causal_prefix_commitment: None,
        }
    }
}

impl std::fmt::Debug for ContinuationPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContinuationPayload")
            .field("version", &self.version)
            .field("provider_response_id", &"<redacted>")
            .field("effective_model", &self.effective_model)
            .field("effective_effort", &self.effective_effort)
            .field("assistant_turn_commitment", &"<redacted>")
            .field("causal_prefix_commitment", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub struct ContinuationKey {
    key_id: String,
    secret: [u8; 32],
}

impl std::fmt::Debug for ContinuationKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContinuationKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl ContinuationKey {
    pub fn from_bytes(secret: [u8; 32]) -> Result<Self> {
        let digest = hmac_bytes(&secret, KEY_ID_DOMAIN, &[])?;
        Ok(Self {
            key_id: format!("continuation-key-{}", hex::encode(&digest[..8])),
            secret,
        })
    }

    fn owner_identity(&self, owner_user_id: &str) -> Result<String> {
        let digest = hmac_bytes(&self.secret, OWNER_IDENTITY_DOMAIN, &[owner_user_id])?;
        Ok(format!("continuation-owner-{}", hex::encode(digest)))
    }

    fn continuation_identity(&self, owner_user_id: &str, request_id: &str) -> Result<String> {
        let digest = hmac_bytes(
            &self.secret,
            CONTINUATION_IDENTITY_DOMAIN,
            &[owner_user_id, request_id],
        )?;
        Ok(format!("continuation-request-{}", hex::encode(digest)))
    }

    fn target_fingerprint(
        &self,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
    ) -> Result<String> {
        let api_base = target
            .api_base_override
            .as_deref()
            .unwrap_or(target.api_base.as_str());
        let account = target.account_label.as_deref().unwrap_or("");
        let protocol = target.api_protocol.to_string();
        let auth_scheme = match credential_authority.effective_scheme() {
            bitrouter_sdk::language_model::types::AuthScheme::XApiKey => "x-api-key",
            bitrouter_sdk::language_model::types::AuthScheme::Bearer => "bearer",
        };
        let credential_authority = hex::encode(credential_authority.credential().proof_bytes());
        let digest = hmac_bytes(
            &self.secret,
            TARGET_FINGERPRINT_DOMAIN,
            &[
                target.provider_name.as_str(),
                account,
                protocol.as_str(),
                api_base,
                auth_scheme,
                credential_authority.as_str(),
            ],
        )?;
        Ok(format!("continuation-target-{}", hex::encode(digest)))
    }
}

fn hmac_bytes(secret: &[u8], domain: &[u8], fields: &[&str]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| anyhow::anyhow!("invalid continuation HMAC key"))?;
    mac.update(domain);
    for field in fields {
        mac.update(&[0]);
        mac.update(field.as_bytes());
    }
    Ok(mac.finalize().into_bytes().to_vec())
}

#[derive(Clone)]
pub struct ContinuationKeySource(Arc<ContinuationKeySourceInner>);

enum ContinuationKeySourceInner {
    Fixed(ContinuationKey),
    Lazy {
        home: PathBuf,
        cached: OnceLock<std::result::Result<ContinuationKey, String>>,
    },
}

impl ContinuationKeySource {
    pub fn fixed(key: ContinuationKey) -> Self {
        Self(Arc::new(ContinuationKeySourceInner::Fixed(key)))
    }

    pub fn lazy(home: PathBuf) -> Self {
        Self(Arc::new(ContinuationKeySourceInner::Lazy {
            home,
            cached: OnceLock::new(),
        }))
    }

    fn load(&self) -> Result<ContinuationKey> {
        match self.0.as_ref() {
            ContinuationKeySourceInner::Fixed(key) => Ok(key.clone()),
            ContinuationKeySourceInner::Lazy { home, cached } => cached
                .get_or_init(|| {
                    crate::paths::get_or_create_continuation_key(home)
                        .and_then(ContinuationKey::from_bytes)
                        .map_err(|error| error.to_string())
                })
                .clone()
                .map_err(anyhow::Error::msg),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationResolution {
    Missing,
    Expired,
    Active(ResolvedContinuation),
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedContinuation {
    pub provider_response_id: String,
    pub effective_model: Option<String>,
    pub effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    /// Whether the sealed payload authoritatively captured an effort value,
    /// including an explicit provider-default (`None`) treatment. Legacy v3
    /// payloads predate effort pinning and must not be reinterpreted as that
    /// provider-default treatment.
    pub effort_authoritative: bool,
    pub causal_prefix_commitment: Option<CausalPrefixCommitment>,
    target_fingerprint: String,
    key: ContinuationKey,
}

impl std::fmt::Debug for ResolvedContinuation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedContinuation")
            .field("provider_response_id", &"<redacted>")
            .field("effective_model", &self.effective_model)
            .field("effective_effort", &self.effective_effort)
            .field("effort_authoritative", &self.effort_authoritative)
            .field("causal_prefix_commitment", &"<redacted>")
            .field("target_fingerprint", &"<redacted>")
            .field("key", &self.key)
            .finish()
    }
}

impl ResolvedContinuation {
    pub fn matches_target(
        &self,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
    ) -> Result<bool> {
        Ok(self.key.target_fingerprint(target, credential_authority)? == self.target_fingerprint)
    }
}

impl PartialEq for ContinuationKey {
    fn eq(&self, other: &Self) -> bool {
        self.key_id == other.key_id && self.secret == other.secret
    }
}

impl Eq for ContinuationKey {}

#[derive(Clone)]
pub struct ContinuationRegistry {
    inner: Arc<ContinuationRegistryInner>,
}

struct ContinuationRegistryInner {
    db: DatabaseConnection,
    keys: ContinuationKeySource,
    retention: TimeDelta,
    prune_batch_size: u64,
    instance_id: String,
    pending_capacity: usize,
    /// Process-local index from delivery attempts to the random generation
    /// authenticated in each durable row. The row's `publication_state` and
    /// `publication_generation` are the durable ownership authority; this map
    /// lets the originating attempt retry an owner-aware CAS compensation.
    ///
    /// A hard process crash after the downstream acknowledgement and durable
    /// activation but before the terminal reaches the socket remains the
    /// intentionally documented at-least-once delivery exception: there is no
    /// process left to perform the compensating delete.
    pending_publications: Arc<Mutex<HashMap<u64, PendingPublication>>>,
    pending_bind_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
    reconciler: ReconcilerControl,
    #[cfg(test)]
    ambiguous_insert_fault: Arc<Mutex<Option<Arc<AmbiguousInsertFault>>>>,
    #[cfg(test)]
    maintenance_fault: Arc<Mutex<Option<Arc<MaintenanceFault>>>>,
}

struct ReconcilerControl {
    notify: Arc<tokio::sync::Notify>,
    task: Mutex<Option<ReconcilerTask>>,
    pending_cursor: Mutex<Option<u64>>,
    stale_offset: Mutex<u64>,
}

struct ReconcilerTask {
    cancelled: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for ContinuationRegistryInner {
    fn drop(&mut self) {
        let task = match self.reconciler.task.get_mut() {
            Ok(task) => task.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(task) = task {
            task.cancelled.store(true, Ordering::Release);
            self.reconciler.notify.notify_waiters();
        }
    }
}

#[cfg(test)]
struct AmbiguousInsertFault {
    committed: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum MaintenanceFaultKind {
    Scrub,
    Purge,
}

#[cfg(test)]
struct MaintenanceFault {
    kind: MaintenanceFaultKind,
    snapshot_read: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[derive(Clone)]
struct PendingPublication {
    continuation_identity: String,
    generation: String,
    instance_id: String,
    lease_until: String,
    receipt: Option<RequiredFinalizationReceipt>,
    phase: PendingPublicationPhase,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingPublicationPhase {
    Reserved,
    Prepared,
    Reconcile,
}

impl PendingPublication {
    fn matches_receipt(&self, receipt: Option<&RequiredFinalizationReceipt>) -> bool {
        match (self.receipt.as_ref(), receipt) {
            (Some(expected), Some(actual)) => expected.same_invocation(actual),
            (None, None) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOutcome {
    Inserted,
    ExistingActive,
    ExistingProvisional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationOwnership {
    Owned,
    LostOrForeign,
    Unknown,
}

struct OwnershipFailure {
    error: anyhow::Error,
    ownership: PublicationOwnership,
}

impl OwnershipFailure {
    fn owned(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            ownership: PublicationOwnership::Owned,
        }
    }

    fn lost_or_foreign(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            ownership: PublicationOwnership::LostOrForeign,
        }
    }

    fn unknown(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            ownership: PublicationOwnership::Unknown,
        }
    }

    fn into_error(self) -> anyhow::Error {
        self.error
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompensationOutcome {
    Released,
    LostOrForeign,
}

struct PendingBindAttempt {
    now: DateTime<Utc>,
    delivery_attempt_id: u64,
    receipt: Option<RequiredFinalizationReceipt>,
}

struct ContinuationBinding<'a> {
    provider_response_id: &'a str,
    effective_model: &'a str,
    effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    causal_prefix_commitment: Option<&'a CausalPrefixCommitment>,
    target: &'a RoutingTarget,
    credential_authority: &'a ContinuationAuthority,
}

struct BindPublication {
    now: DateTime<Utc>,
    state: &'static str,
    generation: String,
    instance_id: String,
    lease_until: String,
}

impl ContinuationRegistry {
    pub fn new(
        db: DatabaseConnection,
        keys: ContinuationKeySource,
        retention_days: u32,
        prune_batch_size: usize,
    ) -> Result<Self> {
        let retention = TimeDelta::try_days(i64::from(retention_days))
            .ok_or_else(|| anyhow::anyhow!("continuation retention_days exceeds time range"))?;
        let prune_batch_size =
            u64::try_from(prune_batch_size).context("continuation prune_batch_size exceeds u64")?;
        Ok(Self {
            inner: Arc::new(ContinuationRegistryInner {
                db,
                keys,
                retention,
                prune_batch_size,
                instance_id: publication_instance_id()?,
                pending_capacity: MAX_PENDING_PUBLICATIONS,
                pending_publications: Arc::new(Mutex::new(HashMap::new())),
                pending_bind_locks: Arc::new(Mutex::new(HashMap::new())),
                reconciler: ReconcilerControl {
                    notify: Arc::new(tokio::sync::Notify::new()),
                    task: Mutex::new(None),
                    pending_cursor: Mutex::new(None),
                    stale_offset: Mutex::new(0),
                },
                #[cfg(test)]
                ambiguous_insert_fault: Arc::new(Mutex::new(None)),
                #[cfg(test)]
                maintenance_fault: Arc::new(Mutex::new(None)),
            }),
        })
    }

    #[cfg(test)]
    fn inject_ambiguous_insert_after_commit(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let fault = Arc::new(AmbiguousInsertFault {
            committed: Mutex::new(Some(committed_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
        });
        match self.inner.ambiguous_insert_fault.lock() {
            Ok(mut installed) => *installed = Some(fault),
            Err(poisoned) => *poisoned.into_inner() = Some(fault),
        }
        (committed_rx, release_tx)
    }

    #[cfg(test)]
    fn pause_maintenance_after_snapshot(
        &self,
        kind: MaintenanceFaultKind,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (snapshot_read_tx, snapshot_read_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let fault = Arc::new(MaintenanceFault {
            kind,
            snapshot_read: Mutex::new(Some(snapshot_read_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
        });
        match self.inner.maintenance_fault.lock() {
            Ok(mut installed) => *installed = Some(fault),
            Err(poisoned) => *poisoned.into_inner() = Some(fault),
        }
        (snapshot_read_rx, release_tx)
    }

    #[cfg(test)]
    async fn wait_at_maintenance_snapshot(&self, kind: MaintenanceFaultKind) {
        let fault = {
            let mut installed = match self.inner.maintenance_fault.lock() {
                Ok(installed) => installed,
                Err(poisoned) => poisoned.into_inner(),
            };
            if installed.as_ref().is_some_and(|fault| fault.kind == kind) {
                installed.take()
            } else {
                None
            }
        };
        let Some(fault) = fault else {
            return;
        };
        let snapshot_read = match fault.snapshot_read.lock() {
            Ok(mut snapshot_read) => snapshot_read.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(snapshot_read) = snapshot_read {
            let _ = snapshot_read.send(());
        }
        if let Some(release) = fault.release.lock().await.take() {
            let _ = release.await;
        }
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.inner.db
    }

    pub(crate) fn start_reconciler(&self) {
        let mut task = match self.inner.reconciler.task.lock() {
            Ok(task) => task,
            Err(poisoned) => poisoned.into_inner(),
        };
        if task
            .as_ref()
            .is_some_and(|running| !running.handle.is_finished())
        {
            self.inner.reconciler.notify.notify_one();
            return;
        }
        let _ = task.take();
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = cancelled.clone();
        let notify = self.inner.reconciler.notify.clone();
        let task_notify = notify.clone();
        let inner = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            let mut delay = RECONCILIATION_BASE_DELAY;
            loop {
                if task_cancelled.load(Ordering::Acquire) {
                    return;
                }
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let registry = ContinuationRegistry { inner };
                let had_error = registry.reconcile_pass(Utc::now()).await;
                let has_pending = registry.has_pending_publications();
                drop(registry);
                delay = if has_pending {
                    Duration::from_secs(1)
                } else if had_error {
                    delay.saturating_mul(2).min(RECONCILIATION_MAX_DELAY)
                } else {
                    RECONCILIATION_MAX_DELAY
                };
                tokio::select! {
                    () = task_notify.notified() => {
                        delay = RECONCILIATION_BASE_DELAY;
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            }
        });
        *task = Some(ReconcilerTask { cancelled, handle });
    }

    async fn stop_reconciler(&self) -> Result<()> {
        let shutdown_deadline = tokio::time::Instant::now() + RECONCILIATION_SHUTDOWN_GRACE;
        let task = {
            let mut task = match self.inner.reconciler.task.lock() {
                Ok(task) => task,
                Err(poisoned) => poisoned.into_inner(),
            };
            task.take()
        };
        if let Some(mut task) = task {
            task.cancelled.store(true, Ordering::Release);
            self.inner.reconciler.notify.notify_waiters();
            match tokio::time::timeout_at(shutdown_deadline, &mut task.handle).await {
                Ok(result) => {
                    result.context("continuation reconciliation worker failed during shutdown")?;
                }
                Err(_) => {
                    task.handle.abort();
                    match task.handle.await {
                        Ok(()) => {}
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => {
                            return Err(error).context(
                                "continuation reconciliation worker failed while being aborted",
                            );
                        }
                    }
                }
            }
        }
        self.mark_pending_for_shutdown_reconciliation();
        loop {
            let before = self.pending_reconciliation_count();
            if before == 0 {
                return Ok(());
            }
            let drain_deadline = tokio::time::Instant::now() + RECONCILIATION_DRAIN_BATCH_TIMEOUT;
            let had_error = tokio::time::timeout_at(
                drain_deadline,
                self.reconcile_local_publications(Utc::now()),
            )
            .await
            .map_err(|_| anyhow::anyhow!("continuation reconciliation drain timed out with {before} retained owner markers"))?;
            let after = self.pending_reconciliation_count();
            if after == 0 {
                return Ok(());
            }
            if after >= before {
                let reason = if had_error {
                    "database reconciliation failed"
                } else {
                    "reconciliation made no progress"
                };
                anyhow::bail!(
                    "continuation reconciliation drain stopped because {reason}; retained {after} owner markers"
                );
            }
        }
    }

    async fn reconcile_pass(&self, now: DateTime<Utc>) -> bool {
        let mut had_error = self.reconcile_local_publications(now).await;
        if self.reconcile_stale_publications(now).await {
            had_error = true;
        }
        had_error
    }

    async fn reconcile_local_publications(&self, now: DateTime<Utc>) -> bool {
        let pending = {
            let publications = match self.inner.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut eligible = publications
                .iter()
                .filter(|(_, publication)| publication.phase != PendingPublicationPhase::Reserved)
                .map(|(delivery_attempt_id, publication)| {
                    (*delivery_attempt_id, publication.clone())
                })
                .collect::<Vec<_>>();
            eligible.sort_unstable_by_key(|(delivery_attempt_id, _)| *delivery_attempt_id);
            if eligible.is_empty() {
                Vec::new()
            } else {
                let mut cursor = match self.inner.reconciler.pending_cursor.lock() {
                    Ok(cursor) => cursor,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let start = cursor
                    .and_then(|previous| {
                        let next =
                            eligible.partition_point(|(attempt_id, _)| *attempt_id <= previous);
                        (next < eligible.len()).then_some(next)
                    })
                    .unwrap_or(0);
                let count = eligible.len().min(RECONCILIATION_BATCH_SIZE);
                let selected = (0..count)
                    .map(|offset| eligible[(start + offset) % eligible.len()].clone())
                    .collect::<Vec<_>>();
                *cursor = selected.last().map(|(attempt_id, _)| *attempt_id);
                selected
            }
        };
        let mut seen = HashSet::new();
        let mut had_error = false;
        for (delivery_attempt_id, snapshot) in pending {
            if !seen.insert((
                snapshot.continuation_identity.clone(),
                snapshot.generation.clone(),
            )) {
                continue;
            }
            let identity_lock = self.pending_identity_lock(&snapshot.continuation_identity);
            let _identity_guard = identity_lock.lock().await;
            let Some(publication) =
                self.pending_publication(delivery_attempt_id, snapshot.receipt.as_ref())
            else {
                continue;
            };
            let publication = match self
                .renew_publication_lease(delivery_attempt_id, &publication, now)
                .await
            {
                Ok(publication) => publication,
                Err(failure) if failure.ownership == PublicationOwnership::LostOrForeign => {
                    self.clear_pending_publication(
                        delivery_attempt_id,
                        &publication.generation,
                        publication.receipt.as_ref(),
                    );
                    continue;
                }
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            if publication.phase != PendingPublicationPhase::Reconcile {
                continue;
            }
            match self.compensate_owned_publication(&publication).await {
                Ok(CompensationOutcome::Released | CompensationOutcome::LostOrForeign) => {
                    self.clear_pending_publication(
                        delivery_attempt_id,
                        &publication.generation,
                        publication.receipt.as_ref(),
                    );
                }
                Err(failure) if failure.ownership == PublicationOwnership::LostOrForeign => {
                    self.clear_pending_publication(
                        delivery_attempt_id,
                        &publication.generation,
                        publication.receipt.as_ref(),
                    );
                }
                Err(_) => had_error = true,
            }
        }
        had_error
    }

    fn mark_pending_for_shutdown_reconciliation(&self) {
        let mut publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        for publication in publications.values_mut() {
            if publication.phase == PendingPublicationPhase::Prepared {
                publication.phase = PendingPublicationPhase::Reconcile;
            }
        }
    }

    fn pending_reconciliation_count(&self) -> usize {
        let publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        publications
            .values()
            .filter(|publication| publication.phase != PendingPublicationPhase::Reserved)
            .count()
    }

    async fn reconcile_stale_publications(&self, now: DateTime<Utc>) -> bool {
        let limit = match u64::try_from(RECONCILIATION_BATCH_SIZE) {
            Ok(limit) => limit,
            Err(_) => return true,
        };
        let offset = {
            let offset = match self.inner.reconciler.stale_offset.lock() {
                Ok(offset) => offset,
                Err(poisoned) => poisoned.into_inner(),
            };
            *offset
        };
        let stale = match continuation_entity::Entity::find()
            .filter(
                continuation_entity::Column::PublicationState
                    .is_in([PUBLICATION_PROVISIONAL, PUBLICATION_DELIVERING]),
            )
            .filter(continuation_entity::Column::PublicationLeaseUntil.lte(timestamp(now)))
            .order_by_asc(continuation_entity::Column::PublicationLeaseUntil)
            .order_by_asc(continuation_entity::Column::ContinuationIdentity)
            .offset(offset)
            .limit(limit)
            .all(&self.inner.db)
            .await
        {
            Ok(stale) => stale,
            Err(_) => return true,
        };
        {
            let mut next_offset = match self.inner.reconciler.stale_offset.lock() {
                Ok(offset) => offset,
                Err(poisoned) => poisoned.into_inner(),
            };
            *next_offset = if stale.len() < RECONCILIATION_BATCH_SIZE {
                0
            } else {
                offset.saturating_add(limit)
            };
        }
        let mut had_error = false;
        for row in stale {
            if self.has_local_generation(&row.continuation_identity, &row.publication_generation) {
                continue;
            }
            let identity_lock = self.pending_identity_lock(&row.continuation_identity);
            let _identity_guard = identity_lock.lock().await;
            match self.claim_and_compensate_stale_publication(row, now).await {
                Ok(()) => {}
                Err(_) => had_error = true,
            }
        }
        had_error
    }

    async fn claim_and_compensate_stale_publication(
        &self,
        row: continuation_entity::Model,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if !matches!(
            row.publication_state.as_str(),
            PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING
        ) || parse_timestamp(&row.publication_lease_until)? > now
        {
            return Ok(());
        }
        let key = self.inner.keys.load()?;
        let plaintext = decrypt_row(&key, &row)?;
        let lease_until = publication_reconciliation_lease_until(now)?;
        let aad = aead_aad([
            &row.key_id,
            &row.owner_identity,
            &row.continuation_identity,
            &row.target_fingerprint,
            &row.created_at,
            &row.expires_at,
            &row.purge_after,
            &row.publication_state,
            &row.publication_generation,
            &self.inner.instance_id,
            &lease_until,
        ]);
        let (ciphertext, nonce) = encrypt(&key, &plaintext, &aad)?;
        let claimed = continuation_entity::Entity::update_many()
            .col_expr(
                continuation_entity::Column::PublicationInstanceId,
                sea_orm::sea_query::Expr::value(self.inner.instance_id.clone()),
            )
            .col_expr(
                continuation_entity::Column::PublicationLeaseUntil,
                sea_orm::sea_query::Expr::value(lease_until.clone()),
            )
            .col_expr(
                continuation_entity::Column::Ciphertext,
                sea_orm::sea_query::Expr::value(ciphertext.clone()),
            )
            .col_expr(
                continuation_entity::Column::Nonce,
                sea_orm::sea_query::Expr::value(nonce.clone()),
            )
            .filter(continuation_snapshot_condition(&row))
            .exec(&self.inner.db)
            .await?;
        if claimed.rows_affected != 1 {
            return Ok(());
        }
        let publication = PendingPublication {
            continuation_identity: row.continuation_identity,
            generation: row.publication_generation,
            instance_id: self.inner.instance_id.clone(),
            lease_until,
            receipt: None,
            phase: PendingPublicationPhase::Reconcile,
        };
        match self.compensate_owned_publication(&publication).await {
            Ok(CompensationOutcome::Released | CompensationOutcome::LostOrForeign) => Ok(()),
            Err(failure) => Err(failure.into_error()),
        }
    }

    fn pending_identity_lock(&self, continuation_identity: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.inner.pending_bind_locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(lock) = locks.get(continuation_identity).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(continuation_identity.to_owned(), Arc::downgrade(&lock));
        lock
    }

    fn pending_publication(
        &self,
        delivery_attempt_id: u64,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> Option<PendingPublication> {
        let publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        publications
            .get(&delivery_attempt_id)
            .filter(|publication| publication.matches_receipt(receipt))
            .cloned()
    }

    async fn rollback_pending(
        &self,
        delivery_attempt_id: u64,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> Result<()> {
        let publication = self.pending_publication(delivery_attempt_id, receipt);
        let Some(publication) = publication else {
            return Ok(());
        };
        // Serialize only the same owner-bound continuation identity. Unrelated
        // callers and request ids retain full DB concurrency.
        let identity_lock = self.pending_identity_lock(&publication.continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        let Some(publication) = self.pending_publication(delivery_attempt_id, receipt) else {
            return Ok(());
        };
        let publication = match self
            .renew_publication_lease(delivery_attempt_id, &publication, Utc::now())
            .await
        {
            Ok(publication) => publication,
            Err(failure) if failure.ownership == PublicationOwnership::LostOrForeign => {
                self.clear_pending_publication(
                    delivery_attempt_id,
                    &publication.generation,
                    receipt,
                );
                return Ok(());
            }
            Err(failure) => {
                self.update_pending_phase(
                    delivery_attempt_id,
                    &publication.generation,
                    receipt,
                    PendingPublicationPhase::Reconcile,
                );
                self.start_reconciler();
                return Err(failure.into_error());
            }
        };
        match self.compensate_owned_publication(&publication).await {
            Ok(CompensationOutcome::Released | CompensationOutcome::LostOrForeign) => {
                self.clear_pending_publication(
                    delivery_attempt_id,
                    &publication.generation,
                    receipt,
                );
                Ok(())
            }
            Err(failure) => {
                if failure.ownership == PublicationOwnership::LostOrForeign {
                    self.clear_pending_publication(
                        delivery_attempt_id,
                        &publication.generation,
                        receipt,
                    );
                } else {
                    self.update_pending_phase(
                        delivery_attempt_id,
                        &publication.generation,
                        receipt,
                        PendingPublicationPhase::Reconcile,
                    );
                    self.start_reconciler();
                }
                Err(failure.into_error())
            }
        }
    }

    fn clear_pending_publication(
        &self,
        delivery_attempt_id: u64,
        generation: &str,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) {
        let mut publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        if publications
            .get(&delivery_attempt_id)
            .is_some_and(|publication| {
                publication.generation == generation && publication.matches_receipt(receipt)
            })
        {
            publications.remove(&delivery_attempt_id);
        }
    }

    fn update_pending_phase(
        &self,
        delivery_attempt_id: u64,
        generation: &str,
        receipt: Option<&RequiredFinalizationReceipt>,
        phase: PendingPublicationPhase,
    ) -> bool {
        let mut publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(publication) = publications.get_mut(&delivery_attempt_id) else {
            return false;
        };
        if publication.generation != generation || !publication.matches_receipt(receipt) {
            return false;
        }
        publication.phase = phase;
        true
    }

    fn update_pending_lease(
        &self,
        delivery_attempt_id: u64,
        previous: &PendingPublication,
        lease_until: String,
    ) -> Option<PendingPublication> {
        let mut publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        let publication = publications.get_mut(&delivery_attempt_id)?;
        if publication.generation != previous.generation
            || publication.instance_id != previous.instance_id
            || publication.lease_until != previous.lease_until
            || !publication.matches_receipt(previous.receipt.as_ref())
        {
            return None;
        }
        publication.lease_until = lease_until;
        Some(publication.clone())
    }

    fn has_pending_publications(&self) -> bool {
        let publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        publications
            .values()
            .any(|publication| publication.phase != PendingPublicationPhase::Reserved)
    }

    fn has_pending_generation(&self, continuation_identity: &str, generation: &str) -> bool {
        let publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        publications.values().any(|publication| {
            publication.phase != PendingPublicationPhase::Reserved
                && publication.continuation_identity == continuation_identity
                && publication.generation == generation
        })
    }

    fn has_local_generation(&self, continuation_identity: &str, generation: &str) -> bool {
        let publications = match self.inner.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        publications.values().any(|publication| {
            publication.continuation_identity == continuation_identity
                && publication.generation == generation
        })
    }

    async fn renew_publication_lease(
        &self,
        delivery_attempt_id: u64,
        publication: &PendingPublication,
        now: DateTime<Utc>,
    ) -> std::result::Result<PendingPublication, OwnershipFailure> {
        let renewal_window = TimeDelta::try_seconds(PUBLICATION_LEASE_SECONDS / 2)
            .ok_or_else(|| OwnershipFailure::owned(anyhow::anyhow!("invalid lease window")))?;
        if parse_timestamp(&publication.lease_until).map_err(OwnershipFailure::owned)?
            > now + renewal_window
        {
            return Ok(publication.clone());
        }
        let row = continuation_entity::Entity::find_by_id(&publication.continuation_identity)
            .one(&self.inner.db)
            .await
            .map_err(OwnershipFailure::unknown)?
            .ok_or_else(|| {
                OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                    "continuation disappeared before lease renewal"
                ))
            })?;
        if !publication_owns_row(publication, &row) {
            return Err(OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                "continuation lease fencing ownership changed"
            )));
        }
        let key = self.inner.keys.load().map_err(OwnershipFailure::owned)?;
        let plaintext = decrypt_row(&key, &row).map_err(OwnershipFailure::owned)?;
        let lease_until = publication_lease_until(now).map_err(OwnershipFailure::owned)?;
        let aad = aead_aad([
            &row.key_id,
            &row.owner_identity,
            &row.continuation_identity,
            &row.target_fingerprint,
            &row.created_at,
            &row.expires_at,
            &row.purge_after,
            &row.publication_state,
            &row.publication_generation,
            &row.publication_instance_id,
            &lease_until,
        ]);
        let (ciphertext, nonce) =
            encrypt(&key, &plaintext, &aad).map_err(OwnershipFailure::owned)?;
        let renewed = continuation_entity::Entity::update_many()
            .col_expr(
                continuation_entity::Column::PublicationLeaseUntil,
                sea_orm::sea_query::Expr::value(lease_until.clone()),
            )
            .col_expr(
                continuation_entity::Column::Ciphertext,
                sea_orm::sea_query::Expr::value(ciphertext),
            )
            .col_expr(
                continuation_entity::Column::Nonce,
                sea_orm::sea_query::Expr::value(nonce),
            )
            .filter(continuation_snapshot_condition(&row))
            .exec(&self.inner.db)
            .await
            .map_err(OwnershipFailure::unknown)?;
        if renewed.rows_affected != 1 {
            return Err(OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                "continuation lease renewal lost fencing ownership"
            )));
        }
        self.update_pending_lease(delivery_attempt_id, publication, lease_until)
            .ok_or_else(|| {
                OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                    "continuation lease marker changed concurrently"
                ))
            })
    }

    async fn compensate_owned_publication(
        &self,
        publication: &PendingPublication,
    ) -> std::result::Result<CompensationOutcome, OwnershipFailure> {
        let row = continuation_entity::Entity::find_by_id(&publication.continuation_identity)
            .one(&self.inner.db)
            .await
            .map_err(OwnershipFailure::unknown)?;
        let Some(mut row) = row else {
            return Ok(CompensationOutcome::LostOrForeign);
        };
        if !publication_owns_row(publication, &row) {
            return Ok(CompensationOutcome::LostOrForeign);
        }
        let key = self.inner.keys.load().map_err(OwnershipFailure::owned)?;
        match row.publication_state.as_str() {
            PUBLICATION_DELIVERING | PUBLICATION_ACTIVE => {
                row = match self
                    .transition_publication_state(&key, &row, PUBLICATION_PROVISIONAL)
                    .await
                {
                    Ok(row) => row,
                    Err(error) => {
                        return self.classify_compensation_error(publication, error).await;
                    }
                };
            }
            PUBLICATION_PROVISIONAL => {
                decrypt_row(&key, &row).map_err(OwnershipFailure::owned)?;
            }
            state => {
                return Err(OwnershipFailure::owned(anyhow::anyhow!(
                    "unsupported continuation publication state '{state}'"
                )));
            }
        }
        let deleted = match continuation_entity::Entity::delete_many()
            .filter(continuation_snapshot_condition(&row))
            .exec(&self.inner.db)
            .await
        {
            Ok(deleted) => deleted,
            Err(error) => {
                return self
                    .classify_compensation_error(publication, error.into())
                    .await;
            }
        };
        if deleted.rows_affected == 1 {
            return Ok(CompensationOutcome::Released);
        }
        self.classify_compensation_error(
            publication,
            anyhow::anyhow!("continuation rollback compare-and-swap lost ownership"),
        )
        .await
    }

    async fn classify_compensation_error(
        &self,
        publication: &PendingPublication,
        error: anyhow::Error,
    ) -> std::result::Result<CompensationOutcome, OwnershipFailure> {
        match continuation_entity::Entity::find_by_id(&publication.continuation_identity)
            .one(&self.inner.db)
            .await
        {
            Ok(None) => Ok(CompensationOutcome::LostOrForeign),
            Ok(Some(row)) if !publication_owns_row(publication, &row) => {
                Ok(CompensationOutcome::LostOrForeign)
            }
            Ok(Some(_)) => Err(OwnershipFailure::owned(error)),
            Err(reread_error) => Err(OwnershipFailure::unknown(anyhow::anyhow!(
                "{error}; rereading continuation ownership after compensation failure: {reread_error}"
            ))),
        }
    }

    async fn activate_pending(
        &self,
        delivery_attempt_id: u64,
        receipt: Option<&RequiredFinalizationReceipt>,
        delivery: &RequiredDeliveryHandshake,
    ) -> Result<bool> {
        let publication = self.pending_publication(delivery_attempt_id, receipt);
        let Some(publication) = publication else {
            // Idempotent preparation against an already-active row owns no
            // durable transition but must still use the delivery rendezvous.
            return delivery
                .wait_for_delivery()
                .await
                .map_err(anyhow::Error::from);
        };
        {
            let identity_lock = self.pending_identity_lock(&publication.continuation_identity);
            let _identity_guard = identity_lock.lock().await;
            let publication = self
                .pending_publication(delivery_attempt_id, receipt)
                .ok_or_else(|| anyhow::anyhow!("provisional continuation ownership disappeared"))?;
            let publication = self
                .renew_publication_lease(delivery_attempt_id, &publication, Utc::now())
                .await
                .map_err(OwnershipFailure::into_error)?;
            let key = self.inner.keys.load()?;
            let row = continuation_entity::Entity::find_by_id(&publication.continuation_identity)
                .one(&self.inner.db)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("provisional continuation disappeared before activation")
                })?;
            if !publication_owns_row(&publication, &row) {
                anyhow::bail!("provisional continuation fencing ownership changed");
            }
            match row.publication_state.as_str() {
                PUBLICATION_PROVISIONAL => {
                    self.transition_publication_state(&key, &row, PUBLICATION_DELIVERING)
                        .await?;
                }
                PUBLICATION_DELIVERING => {
                    decrypt_row(&key, &row)?;
                }
                PUBLICATION_ACTIVE => anyhow::bail!(
                    "continuation publication was already active before delivery acknowledgement"
                ),
                state => anyhow::bail!("unsupported continuation publication state '{state}'"),
            }
        }

        let delivery_result = delivery.wait_for_delivery_acknowledgement().await;
        if delivery_result.as_ref().is_ok_and(|delivered| *delivered) {
            let activation = {
                let identity_lock = self.pending_identity_lock(&publication.continuation_identity);
                let _identity_guard = identity_lock.lock().await;
                let publication = self
                    .pending_publication(delivery_attempt_id, receipt)
                    .ok_or_else(|| {
                        anyhow::anyhow!("delivering continuation ownership disappeared")
                    });
                match publication {
                    Ok(publication) => {
                        let publication = self
                            .renew_publication_lease(delivery_attempt_id, &publication, Utc::now())
                            .await
                            .map_err(OwnershipFailure::into_error)?;
                        let row = continuation_entity::Entity::find_by_id(
                            &publication.continuation_identity,
                        )
                        .one(&self.inner.db)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("delivering continuation disappeared before activation")
                        })?;
                        if !publication_owns_row(&publication, &row) {
                            Err(anyhow::anyhow!(
                                "delivering continuation fencing ownership changed"
                            ))
                        } else if row.publication_state != PUBLICATION_DELIVERING {
                            Err(anyhow::anyhow!(
                                "continuation left delivering state before activation"
                            ))
                        } else {
                            let key = self.inner.keys.load()?;
                            self.transition_publication_state(&key, &row, PUBLICATION_ACTIVE)
                                .await
                        }
                    }
                    Err(error) => Err(error),
                }
            };
            match activation {
                Ok(_) => {
                    let activation_observed = delivery.complete_activation(Ok(()));
                    if activation_observed && delivery.wait_for_terminal_commit().await {
                        self.clear_pending_publication(
                            delivery_attempt_id,
                            &publication.generation,
                            receipt,
                        );
                        return Ok(true);
                    }
                    self.rollback_pending(delivery_attempt_id, receipt).await?;
                    return Ok(false);
                }
                Err(activation_error) => {
                    let compensation = self.rollback_pending(delivery_attempt_id, receipt).await;
                    let error = match compensation {
                        Ok(()) => activation_error,
                        Err(compensation_error) => anyhow::anyhow!(
                            "{activation_error}; compensating failed activation: {compensation_error}"
                        ),
                    };
                    let _ = delivery.complete_activation(Err(BitrouterError::internal(format!(
                        "activating provider continuation publication: {error}"
                    ))));
                    return Err(error);
                }
            }
        }

        match self.rollback_pending(delivery_attempt_id, receipt).await {
            Ok(()) => {
                if delivery.complete_activation(Ok(())) {
                    let _ = delivery.wait_for_terminal_commit().await;
                }
                delivery_result.map_err(anyhow::Error::from)
            }
            Err(error) => {
                let _ = delivery.complete_activation(Err(BitrouterError::internal(format!(
                    "compensating provider continuation publication: {error}"
                ))));
                Err(error)
            }
        }
    }

    async fn transition_publication_state(
        &self,
        key: &ContinuationKey,
        row: &continuation_entity::Model,
        next_state: &'static str,
    ) -> Result<continuation_entity::Model> {
        if !matches!(
            row.publication_state.as_str(),
            PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING | PUBLICATION_ACTIVE
        ) || !matches!(
            next_state,
            PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING | PUBLICATION_ACTIVE
        ) {
            anyhow::bail!("unsupported continuation publication state transition");
        }
        let plaintext = decrypt_row(key, row)?;
        let aad = aead_aad([
            &row.key_id,
            &row.owner_identity,
            &row.continuation_identity,
            &row.target_fingerprint,
            &row.created_at,
            &row.expires_at,
            &row.purge_after,
            next_state,
            &row.publication_generation,
            &row.publication_instance_id,
            &row.publication_lease_until,
        ]);
        let (ciphertext, nonce) = encrypt(key, &plaintext, &aad)?;
        let updated = continuation_entity::Entity::update_many()
            .col_expr(
                continuation_entity::Column::PublicationState,
                sea_orm::sea_query::Expr::value(next_state),
            )
            .col_expr(
                continuation_entity::Column::Ciphertext,
                sea_orm::sea_query::Expr::value(ciphertext.clone()),
            )
            .col_expr(
                continuation_entity::Column::Nonce,
                sea_orm::sea_query::Expr::value(nonce.clone()),
            )
            .filter(continuation_snapshot_condition(row))
            .exec(&self.inner.db)
            .await?;
        if updated.rows_affected != 1 {
            anyhow::bail!("continuation publication state changed concurrently");
        }
        let mut updated_row = row.clone();
        updated_row.publication_state = next_state.to_owned();
        updated_row.ciphertext = Some(ciphertext);
        updated_row.nonce = Some(nonce);
        Ok(updated_row)
    }

    pub async fn bind(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        provider_response_id: &str,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let effective_model = format!("{}:{}", target.provider_name, target.service_id);
        let key = self.inner.keys.load()?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        let outcome = self
            .bind_inner(
                owner_user_id,
                gateway_request_id,
                ContinuationBinding {
                    provider_response_id,
                    effective_model: &effective_model,
                    effective_effort: None,
                    causal_prefix_commitment: None,
                    target,
                    credential_authority,
                },
                BindPublication {
                    now,
                    state: PUBLICATION_ACTIVE,
                    generation: publication_generation()?,
                    instance_id: self.inner.instance_id.clone(),
                    lease_until: publication_lease_until(now)?,
                },
            )
            .await
            .map_err(OwnershipFailure::into_error)?;
        if outcome == BindOutcome::ExistingProvisional {
            anyhow::bail!("gateway continuation id has a pending publication");
        }
        Ok(())
    }

    async fn bind_pending(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        binding: ContinuationBinding<'_>,
        attempt: PendingBindAttempt,
    ) -> Result<bool> {
        let PendingBindAttempt {
            now,
            delivery_attempt_id,
            receipt,
        } = attempt;
        let key = self.inner.keys.load()?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let generation = publication_generation()?;
        let instance_id = self.inner.instance_id.clone();
        let lease_until = publication_lease_until(now)?;
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        {
            let mut publications = match self.inner.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            if publications.len() >= self.inner.pending_capacity
                && !publications.contains_key(&delivery_attempt_id)
            {
                anyhow::bail!("continuation reconciliation capacity exhausted");
            }
            match publications.entry(delivery_attempt_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(PendingPublication {
                        continuation_identity: continuation_identity.clone(),
                        generation: generation.clone(),
                        instance_id: instance_id.clone(),
                        lease_until: lease_until.clone(),
                        receipt: receipt.clone(),
                        phase: PendingPublicationPhase::Reserved,
                    });
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    anyhow::bail!("duplicate continuation delivery attempt id");
                }
            }
        }
        if let Err(error) = self.prune(now).await {
            self.clear_pending_publication(delivery_attempt_id, &generation, receipt.as_ref());
            return Err(error.context("pruning provider continuations"));
        }
        let result = self
            .bind_inner(
                owner_user_id,
                gateway_request_id,
                binding,
                BindPublication {
                    now,
                    state: PUBLICATION_PROVISIONAL,
                    generation: generation.clone(),
                    instance_id,
                    lease_until,
                },
            )
            .await;
        match result {
            Ok(BindOutcome::Inserted) => {
                self.update_pending_phase(
                    delivery_attempt_id,
                    &generation,
                    receipt.as_ref(),
                    PendingPublicationPhase::Prepared,
                );
                self.start_reconciler();
                Ok(true)
            }
            Ok(BindOutcome::ExistingActive) => {
                self.clear_pending_publication(delivery_attempt_id, &generation, receipt.as_ref());
                Ok(false)
            }
            Ok(BindOutcome::ExistingProvisional) => {
                self.clear_pending_publication(delivery_attempt_id, &generation, receipt.as_ref());
                anyhow::bail!("gateway continuation id has a pending publication")
            }
            Err(failure) => {
                if failure.ownership == PublicationOwnership::LostOrForeign {
                    self.clear_pending_publication(
                        delivery_attempt_id,
                        &generation,
                        receipt.as_ref(),
                    );
                } else {
                    self.update_pending_phase(
                        delivery_attempt_id,
                        &generation,
                        receipt.as_ref(),
                        PendingPublicationPhase::Reconcile,
                    );
                    self.start_reconciler();
                }
                // Owned and Unknown retain fenced ownership for automatic
                // reconciliation; conclusively foreign ownership is cleared.
                Err(failure.into_error())
            }
        }
    }

    async fn bind_inner(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        binding: ContinuationBinding<'_>,
        publication: BindPublication,
    ) -> std::result::Result<BindOutcome, OwnershipFailure> {
        let BindPublication {
            now,
            state,
            generation,
            instance_id,
            lease_until,
        } = publication;
        let key = self
            .inner
            .keys
            .load()
            .map_err(OwnershipFailure::lost_or_foreign)?;
        self.ensure_key_epoch(&key, now)
            .await
            .map_err(OwnershipFailure::lost_or_foreign)?;
        let owner_identity = key
            .owner_identity(owner_user_id)
            .map_err(OwnershipFailure::lost_or_foreign)?;
        let continuation_identity = key
            .continuation_identity(owner_user_id, gateway_request_id)
            .map_err(OwnershipFailure::lost_or_foreign)?;
        let target_fingerprint = key
            .target_fingerprint(binding.target, binding.credential_authority)
            .map_err(OwnershipFailure::lost_or_foreign)?;
        let payload = ContinuationPayload::current(
            binding.provider_response_id,
            binding.effective_model,
            binding.effective_effort,
            binding.causal_prefix_commitment,
        )
        .map_err(OwnershipFailure::lost_or_foreign)?;
        let serialized = serde_json::to_vec(&payload)
            .context("serializing continuation payload")
            .map_err(OwnershipFailure::lost_or_foreign)?;
        let mut plaintext = CONTINUATION_PAYLOAD_V4_MAGIC.to_vec();
        plaintext.extend_from_slice(&serialized);
        let created_at = timestamp(now);
        let expires_at = timestamp(
            now.checked_add_signed(self.inner.retention)
                .ok_or_else(|| anyhow::anyhow!("continuation expiry exceeds time range"))
                .map_err(OwnershipFailure::lost_or_foreign)?,
        );
        let purge_after = timestamp(
            now.checked_add_signed(self.inner.retention + self.inner.retention)
                .ok_or_else(|| anyhow::anyhow!("continuation purge boundary exceeds time range"))
                .map_err(OwnershipFailure::lost_or_foreign)?,
        );
        let aad = aead_aad([
            &key.key_id,
            &owner_identity,
            &continuation_identity,
            &target_fingerprint,
            &created_at,
            &expires_at,
            &purge_after,
            state,
            &generation,
            &instance_id,
            &lease_until,
        ]);
        let (ciphertext, nonce) =
            encrypt(&key, &plaintext, &aad).map_err(OwnershipFailure::lost_or_foreign)?;
        let model = continuation_entity::ActiveModel {
            continuation_identity: Set(continuation_identity.clone()),
            owner_identity: Set(owner_identity),
            ciphertext: Set(Some(ciphertext)),
            nonce: Set(Some(nonce)),
            target_fingerprint: Set(target_fingerprint.clone()),
            key_id: Set(key.key_id.clone()),
            cipher_version: Set(CIPHER_VERSION),
            created_at: Set(created_at),
            expires_at: Set(expires_at),
            purge_after: Set(purge_after),
            publication_state: Set(state.to_owned()),
            publication_generation: Set(generation.clone()),
            publication_instance_id: Set(instance_id.clone()),
            publication_lease_until: Set(lease_until.clone()),
        };
        let insert_result = model.insert(&self.inner.db).await;
        #[cfg(test)]
        let insert_result = match insert_result {
            Ok(model) => {
                let fault = match self.inner.ambiguous_insert_fault.lock() {
                    Ok(mut fault) => fault.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                if let Some(fault) = fault {
                    let committed = match fault.committed.lock() {
                        Ok(mut committed) => committed.take(),
                        Err(poisoned) => poisoned.into_inner().take(),
                    };
                    if let Some(committed) = committed {
                        let _ = committed.send(());
                    }
                    if let Some(release) = fault.release.lock().await.take() {
                        let _ = release.await;
                    }
                    Err(sea_orm::DbErr::Custom(
                        "injected ambiguous insert result after commit".to_owned(),
                    ))
                } else {
                    Ok(model)
                }
            }
            Err(error) => Err(error),
        };
        match insert_result {
            Ok(_) => Ok(BindOutcome::Inserted),
            Err(insert_error) => {
                let existing = continuation_entity::Entity::find_by_id(&continuation_identity)
                    .one(&self.inner.db)
                    .await
                    .map_err(|reread_error| {
                        OwnershipFailure::unknown(anyhow::anyhow!(
                            "inserting provider continuation failed: {insert_error}; rereading publication ownership failed: {reread_error}"
                        ))
                    })?;
                let Some(existing) = existing else {
                    return Err(OwnershipFailure::lost_or_foreign(insert_error));
                };
                if existing.publication_generation == generation
                    && existing.publication_instance_id == instance_id
                    && existing.publication_lease_until == lease_until
                {
                    let existing_payload =
                        decrypt_payload(&key, &existing).map_err(OwnershipFailure::owned)?;
                    if existing_payload != payload
                        || existing.target_fingerprint != target_fingerprint
                    {
                        return Err(OwnershipFailure::owned(anyhow::anyhow!(
                            "gateway continuation id is already bound to another response"
                        )));
                    }
                    return match existing.publication_state.as_str() {
                        PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING | PUBLICATION_ACTIVE => {
                            Ok(BindOutcome::Inserted)
                        }
                        state => Err(OwnershipFailure::owned(anyhow::anyhow!(
                            "unsupported continuation publication state '{state}'"
                        ))),
                    };
                }

                let existing_payload =
                    decrypt_payload(&key, &existing).map_err(OwnershipFailure::lost_or_foreign)?;
                if existing_payload != payload || existing.target_fingerprint != target_fingerprint
                {
                    return Err(OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                        "gateway continuation id is already bound to another response"
                    )));
                }
                match existing.publication_state.as_str() {
                    PUBLICATION_ACTIVE => Ok(BindOutcome::ExistingActive),
                    PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING => {
                        Ok(BindOutcome::ExistingProvisional)
                    }
                    state => Err(OwnershipFailure::lost_or_foreign(anyhow::anyhow!(
                        "unsupported continuation publication state '{state}'"
                    ))),
                }
            }
        }
    }

    pub async fn resolve(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ContinuationResolution> {
        let key = self.inner.keys.load()?;
        self.ensure_key_epoch(&key, now).await?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        // The same-identity lock linearizes resolve with bind, activation, and
        // rollback, including the old precheck-to-insert TOCTOU window.
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _identity_guard = identity_lock.lock().await;
        loop {
            let Some(row) = continuation_entity::Entity::find_by_id(&continuation_identity)
                .one(&self.inner.db)
                .await?
            else {
                return Ok(ContinuationResolution::Missing);
            };
            if row.publication_state != PUBLICATION_ACTIVE {
                return Ok(ContinuationResolution::Missing);
            }
            if self.has_pending_generation(&row.continuation_identity, &row.publication_generation)
            {
                return Ok(ContinuationResolution::Missing);
            }
            let expected_owner = key.owner_identity(owner_user_id)?;
            if row.owner_identity != expected_owner {
                anyhow::bail!("continuation owner identity mismatch");
            }
            if row.key_id != key.key_id {
                anyhow::bail!("continuation key epoch mismatch");
            }
            if row.cipher_version != CIPHER_VERSION {
                anyhow::bail!("unsupported continuation cipher version");
            }
            let expires_at = parse_timestamp(&row.expires_at)?;
            if now >= expires_at {
                if self.scrub_expired(&row).await? {
                    return Ok(ContinuationResolution::Expired);
                }
                // A different generation replaced the stale snapshot. Reload
                // it rather than reporting or mutating the superseded row.
                continue;
            }
            let payload = decrypt_payload(&key, &row)?;
            return Ok(ContinuationResolution::Active(ResolvedContinuation {
                provider_response_id: payload.provider_response_id,
                effective_model: payload.effective_model,
                effective_effort: payload.effective_effort,
                effort_authoritative: payload.version >= 4,
                causal_prefix_commitment: payload
                    .causal_prefix_commitment
                    .as_deref()
                    .and_then(CausalPrefixCommitment::parse),
                target_fingerprint: row.target_fingerprint,
                key,
            }));
        }
    }

    pub async fn prune(&self, now: DateTime<Utc>) -> Result<u64> {
        let now = timestamp(now);
        let expired = continuation_entity::Entity::find()
            .filter(continuation_entity::Column::ExpiresAt.lte(now.clone()))
            .filter(continuation_entity::Column::Ciphertext.is_not_null())
            .order_by_asc(continuation_entity::Column::ExpiresAt)
            .limit(self.inner.prune_batch_size)
            .all(&self.inner.db)
            .await?;
        for row in expired {
            self.scrub_expired(&row).await?;
        }

        let purge_rows = continuation_entity::Entity::find()
            .filter(continuation_entity::Column::PurgeAfter.lte(now))
            .order_by_asc(continuation_entity::Column::PurgeAfter)
            .limit(self.inner.prune_batch_size)
            .all(&self.inner.db)
            .await?;
        #[cfg(test)]
        self.wait_at_maintenance_snapshot(MaintenanceFaultKind::Purge)
            .await;
        let mut rows_affected = 0;
        for row in purge_rows {
            let result = continuation_entity::Entity::delete_many()
                .filter(continuation_snapshot_condition(&row))
                .exec(&self.inner.db)
                .await?;
            rows_affected += result.rows_affected;
        }
        Ok(rows_affected)
    }

    async fn scrub_expired(&self, row: &continuation_entity::Model) -> Result<bool> {
        #[cfg(test)]
        self.wait_at_maintenance_snapshot(MaintenanceFaultKind::Scrub)
            .await;
        let result = continuation_entity::Entity::update_many()
            .col_expr(
                continuation_entity::Column::Ciphertext,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .col_expr(
                continuation_entity::Column::Nonce,
                sea_orm::sea_query::Expr::value(Option::<String>::None),
            )
            .filter(continuation_snapshot_condition(row))
            .exec(&self.inner.db)
            .await?;
        Ok(result.rows_affected == 1)
    }

    async fn ensure_key_epoch(&self, key: &ContinuationKey, now: DateTime<Utc>) -> Result<()> {
        let existing = key_epoch_entity::Entity::find_by_id(1)
            .one(&self.inner.db)
            .await?;
        let epoch = match existing {
            Some(epoch) => epoch,
            None => {
                let insert = key_epoch_entity::ActiveModel {
                    singleton_id: Set(1),
                    key_id: Set(key.key_id.clone()),
                    created_at: Set(timestamp(now)),
                };
                match insert.insert(&self.inner.db).await {
                    Ok(epoch) => epoch,
                    Err(_) => key_epoch_entity::Entity::find_by_id(1)
                        .one(&self.inner.db)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("continuation key epoch could not be initialized")
                        })?,
                }
            }
        };
        if epoch.key_id != key.key_id {
            anyhow::bail!("continuation key epoch mismatch");
        }
        Ok(())
    }
}

fn continuation_snapshot_condition(row: &continuation_entity::Model) -> sea_orm::Condition {
    let condition = sea_orm::Condition::all()
        .add(
            continuation_entity::Column::ContinuationIdentity.eq(row.continuation_identity.clone()),
        )
        .add(continuation_entity::Column::OwnerIdentity.eq(row.owner_identity.clone()))
        .add(continuation_entity::Column::TargetFingerprint.eq(row.target_fingerprint.clone()))
        .add(continuation_entity::Column::KeyId.eq(row.key_id.clone()))
        .add(continuation_entity::Column::CipherVersion.eq(row.cipher_version))
        .add(continuation_entity::Column::CreatedAt.eq(row.created_at.clone()))
        .add(continuation_entity::Column::ExpiresAt.eq(row.expires_at.clone()))
        .add(continuation_entity::Column::PurgeAfter.eq(row.purge_after.clone()))
        .add(continuation_entity::Column::PublicationState.eq(row.publication_state.clone()))
        .add(
            continuation_entity::Column::PublicationGeneration
                .eq(row.publication_generation.clone()),
        )
        .add(
            continuation_entity::Column::PublicationInstanceId
                .eq(row.publication_instance_id.clone()),
        )
        .add(
            continuation_entity::Column::PublicationLeaseUntil
                .eq(row.publication_lease_until.clone()),
        );
    let condition = match &row.ciphertext {
        Some(ciphertext) => {
            condition.add(continuation_entity::Column::Ciphertext.eq(ciphertext.clone()))
        }
        None => condition.add(continuation_entity::Column::Ciphertext.is_null()),
    };
    match &row.nonce {
        Some(nonce) => condition.add(continuation_entity::Column::Nonce.eq(nonce.clone())),
        None => condition.add(continuation_entity::Column::Nonce.is_null()),
    }
}

fn publication_owns_row(
    publication: &PendingPublication,
    row: &continuation_entity::Model,
) -> bool {
    row.publication_generation == publication.generation
        && row.publication_instance_id == publication.instance_id
        && row.publication_lease_until == publication.lease_until
}

/// Always-on route resolver and success-critical continuation publisher.
#[derive(Clone)]
pub struct ContinuationRuntime {
    registry: ContinuationRegistry,
    auth_appliers: AuthAppliers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContinuationAdjustment {
    Pin {
        effective_model: String,
        effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
        effort_authoritative: bool,
    },
    Detach,
    RejectLegacy,
}

impl ContinuationAdjustment {
    pub(crate) fn pinned_effort_override(
        &self,
    ) -> Option<Option<bitrouter_sdk::language_model::types::ReasoningEffort>> {
        match self {
            Self::Pin {
                effective_effort,
                effort_authoritative,
                ..
            } => effort_authoritative.then_some(*effective_effort),
            Self::Detach | Self::RejectLegacy => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ContinuationRequestPlan {
    pub(crate) adjustment: Option<ContinuationAdjustment>,
    active: ResolvedContinuation,
}

#[derive(Debug)]
struct RejectContinuationPreflight(&'static str);

impl ContinuationRuntime {
    pub fn new(registry: ContinuationRegistry) -> Self {
        Self {
            registry,
            auth_appliers: AuthAppliers::new(),
        }
    }

    pub fn with_auth_appliers(registry: ContinuationRegistry, auth_appliers: AuthAppliers) -> Self {
        Self {
            registry,
            auth_appliers,
        }
    }

    pub fn registry(&self) -> &ContinuationRegistry {
        &self.registry
    }
}

#[async_trait]
impl PreRequestHook for ContinuationRuntime {
    async fn check(&self, ctx: &mut PipelineContext) -> PipelineResult<HookDecision> {
        if ctx.inbound_protocol() != Some(ApiProtocol::Responses) {
            return Ok(HookDecision::Allow);
        }
        let Some(previous_response_id) = ctx
            .prompt()
            .params
            .extra
            .get("previous_response_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(HookDecision::Allow);
        };
        if decode_gateway_continuation_id(&previous_response_id)?.is_none() {
            let explicit_provider_route =
                ctx.model()
                    .split_once(':')
                    .is_some_and(|(provider, model)| {
                        !provider.is_empty() && !model.is_empty() && !provider.starts_with('@')
                    });
            if !explicit_provider_route {
                ctx.insert_extension(Arc::new(RejectContinuationPreflight(
                    "native provider continuation requires an explicit provider model",
                )));
            }
            return Ok(HookDecision::Allow);
        }
        let resolution = self
            .registry
            .resolve(ctx.caller().user_id(), &previous_response_id, Utc::now())
            .await
            .map_err(|error| {
                BitrouterError::internal(format!(
                    "resolving provider continuation failed closed: {error}"
                ))
            })?;
        let active = match resolution {
            ContinuationResolution::Active(active) => active,
            ContinuationResolution::Expired => {
                ctx.insert_extension(Arc::new(RejectContinuationPreflight(
                    "provider continuation has expired",
                )));
                return Ok(HookDecision::Allow);
            }
            ContinuationResolution::Missing => {
                ctx.insert_extension(Arc::new(RejectContinuationPreflight(
                    "gateway continuation mapping is unavailable",
                )));
                return Ok(HookDecision::Allow);
            }
        };
        let adjustment = match active.effective_model.clone() {
            None => {
                ctx.insert_extension(Arc::new(RejectContinuationPreflight(
                    "legacy provider continuation has no authenticated model provenance",
                )));
                Some(ContinuationAdjustment::RejectLegacy)
            }
            Some(effective_model) => {
                let exact_visible_prefix =
                    active
                        .causal_prefix_commitment
                        .as_ref()
                        .and_then(|commitment| {
                            authenticated_visible_causal_prefix(ctx.prompt(), commitment)
                        });
                if let (Some(commitment), Some(suffix_start)) = (
                    active.causal_prefix_commitment.clone(),
                    exact_visible_prefix,
                ) {
                    ctx.insert_extension(Arc::new(CausalPrefixPlan::extend(
                        commitment,
                        suffix_start,
                    )));
                    ctx.insert_extension(Arc::new(SuppressProviderContinuation));
                    Some(ContinuationAdjustment::Detach)
                } else {
                    let hidden_suffix = !ctx
                        .prompt()
                        .messages
                        .iter()
                        .any(|message| message.role == Role::Assistant);
                    let causal_plan = active
                        .causal_prefix_commitment
                        .clone()
                        .filter(|_| hidden_suffix)
                        .map_or_else(CausalPrefixPlan::ineligible, |commitment| {
                            CausalPrefixPlan::extend(commitment, 0)
                        });
                    ctx.insert_extension(Arc::new(causal_plan));
                    Some(ContinuationAdjustment::Pin {
                        effective_model,
                        effective_effort: active.effective_effort,
                        effort_authoritative: active.effort_authoritative,
                    })
                }
            }
        };
        ctx.insert_extension(Arc::new(ContinuationRequestPlan { adjustment, active }));
        Ok(HookDecision::Allow)
    }
}

#[async_trait]
impl RouteHook for ContinuationRuntime {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> PipelineResult<()> {
        if ctx.inbound_protocol() != Some(ApiProtocol::Responses) {
            return Ok(());
        }
        ctx.insert_extension(Arc::new(RequireContinuationAuthority));
        let Some(previous_response_id) = ctx
            .prompt()
            .params
            .extra
            .get("previous_response_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(());
        };
        let decoded_gateway_id = decode_gateway_continuation_id(&previous_response_id)?;
        if let Some(rejection) = ctx.extension::<RejectContinuationPreflight>() {
            return Err(BitrouterError::bad_request(rejection.0));
        }
        if decoded_gateway_id.is_none() {
            let selected = chain
                .iter()
                .find(|target| target.api_protocol == ApiProtocol::Responses)
                .cloned()
                .ok_or_else(|| {
                    BitrouterError::bad_request(
                        "native provider continuation has no Responses target",
                    )
                })?;
            chain.clear();
            chain.push(selected);
            return Ok(());
        }
        if ctx
            .extension::<ContinuationRequestPlan>()
            .is_some_and(|plan| {
                matches!(
                    plan.adjustment.as_ref(),
                    Some(ContinuationAdjustment::Detach)
                )
            })
        {
            let selected = chain
                .iter()
                .find(|target| target.api_protocol == ApiProtocol::Responses)
                .cloned()
                .ok_or_else(|| {
                    BitrouterError::bad_request(
                        "detached provider continuation has no Responses target",
                    )
                })?;
            chain.clear();
            chain.push(selected);
            return Ok(());
        }
        let prepared = ctx.extension::<ContinuationRequestPlan>();
        let resolution = if let Some(prepared) = prepared {
            ContinuationResolution::Active(prepared.active.clone())
        } else {
            self.registry
                .resolve(ctx.caller().user_id(), &previous_response_id, Utc::now())
                .await
                .map_err(|error| {
                    BitrouterError::internal(format!(
                        "resolving provider continuation failed closed: {error}"
                    ))
                })?
        };
        match resolution {
            ContinuationResolution::Active(active) => {
                if active.effective_model.is_none() {
                    return Err(BitrouterError::bad_request(
                        "legacy provider continuation has no authenticated model provenance",
                    ));
                }
                let mut selected = None;
                for target in chain.iter() {
                    if target.api_protocol != ApiProtocol::Responses {
                        continue;
                    }
                    let credential_authority = self
                        .auth_appliers
                        .continuation_authority_proof(target)
                        .await
                        .map_err(|error| {
                            BitrouterError::internal(format!(
                                "resolving provider continuation authority: {error}"
                            ))
                        })?;
                    let Some(credential_authority) = credential_authority else {
                        continue;
                    };
                    if active
                        .matches_target(target, &credential_authority)
                        .map_err(|error| {
                            BitrouterError::internal(format!(
                                "validating provider continuation target: {error}"
                            ))
                        })?
                    {
                        selected = Some((target.clone(), credential_authority));
                        break;
                    }
                }
                let (selected, credential_authority) = selected.ok_or_else(|| {
                    BitrouterError::bad_request(
                        "provider continuation target is unavailable or changed",
                    )
                })?;
                chain.clear();
                chain.push(selected.clone());
                ctx.insert_extension(Arc::new(ProviderContinuation::new(
                    active.provider_response_id,
                    &selected,
                    credential_authority,
                )));
                Ok(())
            }
            ContinuationResolution::Expired => Err(BitrouterError::bad_request(
                "provider continuation has expired",
            )),
            ContinuationResolution::Missing => Err(BitrouterError::bad_request(
                "gateway continuation mapping is unavailable",
            )),
        }
    }
}

impl ContinuationRuntime {
    async fn finalize_required(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> PipelineResult<()> {
        if !ctx.successful_terminal || ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return Ok(());
        }
        if !ctx.native_response_completed {
            return Ok(());
        }
        let Some(target) = ctx.target.as_ref() else {
            return Err(BitrouterError::internal(
                "successful Responses continuation has no serving target",
            ));
        };
        if target.api_protocol != ApiProtocol::Responses {
            return Ok(());
        }
        let provider_response_id = ctx.response_id.as_deref().ok_or_else(|| {
            BitrouterError::internal(
                "native Responses completion did not provide a continuation id",
            )
        })?;
        let credential_authority = match ctx.credential_authority.as_ref() {
            Some(authority) => authority.clone(),
            None if self.auth_appliers.lookup(&target.provider_name).is_none() => self
                .auth_appliers
                .continuation_authority_proof(target)
                .await
                .map_err(|error| {
                    BitrouterError::internal(format!(
                        "resolving static continuation authority: {error}"
                    ))
                })?
                .ok_or_else(|| BitrouterError::internal("continuation authority unavailable"))?,
            None => {
                return Err(BitrouterError::internal(
                    "continuation authority unavailable for dynamic authentication",
                ));
            }
        };
        let public_continuation_id = encode_gateway_continuation_id(&ctx.request_id)?;
        let now = Utc::now();
        self.registry
            .bind_pending(
                ctx.caller.user_id(),
                &public_continuation_id,
                ContinuationBinding {
                    provider_response_id,
                    effective_model: &ctx.effective_model,
                    effective_effort: ctx.effective_effort,
                    causal_prefix_commitment: ctx.causal_prefix_commitment.as_ref(),
                    target,
                    credential_authority: &credential_authority,
                },
                PendingBindAttempt {
                    now,
                    delivery_attempt_id: ctx.delivery_attempt_id,
                    receipt: receipt.cloned(),
                },
            )
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("persisting provider continuation: {error}"))
            })?;
        Ok(())
    }

    async fn rollback_required(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> PipelineResult<()> {
        if ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return Ok(());
        }
        self.registry
            .rollback_pending(ctx.delivery_attempt_id, receipt)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!(
                    "rolling back provider continuation publication: {error}"
                ))
            })
    }

    async fn commit_required(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: Option<&RequiredFinalizationReceipt>,
        delivery: &RequiredDeliveryHandshake,
    ) -> PipelineResult<bool> {
        if ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return delivery.wait_for_delivery().await;
        }
        match self
            .registry
            .activate_pending(ctx.delivery_attempt_id, receipt, delivery)
            .await
        {
            Ok(delivered) => Ok(delivered),
            Err(error) => {
                let error = BitrouterError::internal(format!(
                    "activating provider continuation publication: {error}"
                ));
                delivery.reject(error.clone());
                Err(error)
            }
        }
    }
}

#[async_trait]
impl RequiredFinalizer for ContinuationRuntime {
    async fn finalize(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
        self.finalize_required(ctx, None).await
    }

    async fn finalize_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
    ) -> PipelineResult<()> {
        self.finalize_required(ctx, Some(receipt)).await
    }

    async fn rollback(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
        self.rollback_required(ctx, None).await
    }

    async fn rollback_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
    ) -> PipelineResult<()> {
        self.rollback_required(ctx, Some(receipt)).await
    }

    async fn commit(
        &self,
        ctx: &RequiredFinalizationContext,
        delivery: &RequiredDeliveryHandshake,
    ) -> PipelineResult<bool> {
        self.commit_required(ctx, None, delivery).await
    }

    async fn commit_with_receipt(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: &RequiredFinalizationReceipt,
        delivery: &RequiredDeliveryHandshake,
    ) -> PipelineResult<bool> {
        self.commit_required(ctx, Some(receipt), delivery).await
    }

    async fn drain_pending_work(&self) -> PipelineResult<()> {
        self.registry.stop_reconciler().await.map_err(|error| {
            BitrouterError::internal(format!("draining continuation reconciliation: {error}"))
        })
    }
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("invalid continuation timestamp '{value}'"))
}

fn aead_aad(fields: [&str; 11]) -> Vec<u8> {
    let mut aad = AEAD_AAD_DOMAIN.to_vec();
    for field in fields {
        aad.push(0);
        aad.extend_from_slice(field.as_bytes());
    }
    aad
}

fn encrypt(key: &ContinuationKey, plaintext: &[u8], aad: &[u8]) -> Result<(String, String)> {
    let unbound = UnboundKey::new(&AES_256_GCM, &key.secret)
        .map_err(|_| anyhow::anyhow!("invalid continuation encryption key"))?;
    let key = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0_u8; NONCE_BYTES];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("continuation nonce generation failed"))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::from(aad), &mut ciphertext)
        .map_err(|_| anyhow::anyhow!("continuation encryption failed"))?;
    Ok((
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ciphertext),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes),
    ))
}

fn publication_generation() -> Result<String> {
    let mut generation = [0_u8; PUBLICATION_GENERATION_BYTES];
    SystemRandom::new()
        .fill(&mut generation)
        .map_err(|_| anyhow::anyhow!("continuation publication generation failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(generation))
}

fn publication_instance_id() -> Result<String> {
    let mut instance = [0_u8; PUBLICATION_INSTANCE_BYTES];
    SystemRandom::new()
        .fill(&mut instance)
        .map_err(|_| anyhow::anyhow!("continuation publication instance generation failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(instance))
}

fn publication_lease_until(now: DateTime<Utc>) -> Result<String> {
    publication_lease_until_for(now, PUBLICATION_LEASE_SECONDS)
}

fn publication_reconciliation_lease_until(now: DateTime<Utc>) -> Result<String> {
    publication_lease_until_for(now, PUBLICATION_RECONCILIATION_LEASE_SECONDS)
}

fn publication_lease_until_for(now: DateTime<Utc>, seconds: i64) -> Result<String> {
    let duration = TimeDelta::try_seconds(seconds)
        .ok_or_else(|| anyhow::anyhow!("continuation publication lease exceeds time range"))?;
    let lease_until = now
        .checked_add_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("continuation publication lease exceeds time range"))?;
    Ok(timestamp(lease_until))
}

fn decrypt_row(key: &ContinuationKey, row: &continuation_entity::Model) -> Result<Vec<u8>> {
    let ciphertext = row
        .ciphertext
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
    let nonce = row
        .nonce
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
    let mut ciphertext = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(ciphertext)
        .context("continuation ciphertext is not valid base64")?;
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(nonce)
        .context("continuation nonce is not valid base64")?;
    let nonce: [u8; NONCE_BYTES] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("continuation nonce has invalid length"))?;
    let aad = aead_aad([
        &row.key_id,
        &row.owner_identity,
        &row.continuation_identity,
        &row.target_fingerprint,
        &row.created_at,
        &row.expires_at,
        &row.purge_after,
        &row.publication_state,
        &row.publication_generation,
        &row.publication_instance_id,
        &row.publication_lease_until,
    ]);
    let unbound = UnboundKey::new(&AES_256_GCM, &key.secret)
        .map_err(|_| anyhow::anyhow!("invalid continuation encryption key"))?;
    let key = LessSafeKey::new(unbound);
    let plaintext = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad.as_slice()),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("continuation authentication failed"))?;
    Ok(plaintext.to_vec())
}

fn decrypt_payload(
    key: &ContinuationKey,
    row: &continuation_entity::Model,
) -> Result<ContinuationPayload> {
    let plaintext = decrypt_row(key, row)?;
    let payload = if let Some(serialized) = plaintext.strip_prefix(CONTINUATION_PAYLOAD_V4_MAGIC) {
        let payload: ContinuationPayload =
            serde_json::from_slice(serialized).context("continuation v4 payload is invalid")?;
        if payload.version != 4 {
            anyhow::bail!("unsupported continuation v4 payload version");
        }
        payload
    } else if let Some(serialized) = plaintext.strip_prefix(CONTINUATION_PAYLOAD_V3_MAGIC) {
        let payload: ContinuationPayload =
            serde_json::from_slice(serialized).context("continuation v3 payload is invalid")?;
        if payload.version != 3 {
            anyhow::bail!("unsupported continuation v3 payload version");
        }
        payload
    } else if let Some(serialized) = plaintext.strip_prefix(CONTINUATION_PAYLOAD_V2_MAGIC) {
        let mut payload: ContinuationPayload =
            serde_json::from_slice(serialized).context("continuation v2 payload is invalid")?;
        if payload.version != 2 {
            anyhow::bail!("unsupported continuation v2 payload version");
        }
        payload.causal_prefix_commitment = None;
        payload
    } else {
        let plaintext = String::from_utf8(plaintext)
            .context("legacy continuation plaintext is not valid UTF-8")?;
        // V1 used a valid UTF-8 magic prefix and can collide with a provider's
        // opaque native id. Preserve decode compatibility as legacy data, but
        // never infer model provenance from the colliding plaintext.
        return Ok(ContinuationPayload::legacy(plaintext));
    };
    let effective_model = payload
        .effective_model
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("continuation payload has no effective model"))?;
    validate_effective_model(effective_model)?;
    if let Some(commitment) = payload.assistant_turn_commitment.as_deref()
        && AssistantTurnCommitment::parse(commitment).is_none()
    {
        anyhow::bail!("continuation assistant-turn commitment is invalid");
    }
    if let Some(commitment) = payload.causal_prefix_commitment.as_deref()
        && CausalPrefixCommitment::parse(commitment).is_none()
    {
        anyhow::bail!("continuation causal-prefix commitment is invalid");
    }
    Ok(payload)
}

fn validate_effective_model(effective_model: &str) -> Result<()> {
    if effective_model.is_empty()
        || effective_model.len() > MAX_EFFECTIVE_MODEL_BYTES
        || effective_model.chars().any(char::is_control)
    {
        anyhow::bail!("continuation effective model is invalid");
    }
    Ok(())
}

mod continuation_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "provider_continuations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub continuation_identity: String,
        pub owner_identity: String,
        pub ciphertext: Option<String>,
        pub nonce: Option<String>,
        pub target_fingerprint: String,
        pub key_id: String,
        pub cipher_version: i32,
        pub created_at: String,
        pub expires_at: String,
        pub purge_after: String,
        pub publication_state: String,
        pub publication_generation: String,
        pub publication_instance_id: String,
        pub publication_lease_until: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod key_epoch_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "provider_continuation_key_epoch")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub singleton_id: i32,
        pub key_id: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::auth::{AppliedAuth, AuthApplier, CredentialAuthority};
    use bitrouter_sdk::language_model::context::{PipelineContext, ProviderContinuation};
    use bitrouter_sdk::language_model::hooks::{
        ObserveHook, RequestOutcome, RouteHook, StreamHook,
    };
    use bitrouter_sdk::language_model::settlement::{
        DeliveryAcknowledgement, RequiredFinalizationContext, RequiredFinalizer, SettlementContext,
        SettlementRecorder,
    };
    use bitrouter_sdk::language_model::{
        ApiProtocol, AuthAppliers, Content, ExecutionResult, Executor, FinishReason,
        GenerateResult, GenerationParams, HttpExecutor, Message, MockExecutor, MockResponse,
        Pipeline, PipelineBuilder, PipelineRequest, Prompt, Role, RoutingTarget,
        StaticRoutingTable, StreamAction, StreamContext, StreamInterest, StreamOutcome, StreamPart,
        StreamPartStream, Tool, ToolResultOutput, Usage,
    };
    use bitrouter_sdk::server::{AppState, build_router};
    use chrono::{TimeDelta, TimeZone, Utc};
    use futures::StreamExt;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    #[test]
    fn continuation_pin_distinguishes_provider_default_from_no_override() {
        let provider_default = ContinuationAdjustment::Pin {
            effective_model: "openai:gpt-5.4".into(),
            effective_effort: None,
            effort_authoritative: true,
        };
        let high = ContinuationAdjustment::Pin {
            effective_model: "openai:gpt-5.4".into(),
            effective_effort: Some(bitrouter_sdk::language_model::types::ReasoningEffort::High),
            effort_authoritative: true,
        };
        let legacy = ContinuationAdjustment::Pin {
            effective_model: "openai:gpt-5.4".into(),
            effective_effort: None,
            effort_authoritative: false,
        };

        assert_eq!(provider_default.pinned_effort_override(), Some(None));
        assert_eq!(
            high.pinned_effort_override(),
            Some(Some(
                bitrouter_sdk::language_model::types::ReasoningEffort::High
            ))
        );
        assert_eq!(
            ContinuationAdjustment::Detach.pinned_effort_override(),
            None
        );
        assert_eq!(legacy.pinned_effort_override(), None);
    }

    fn target(api_key: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: "openai".into(),
            service_id: "gpt-5".into(),
            api_base: "https://api.openai.example/v1".into(),
            api_key: api_key.into(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: Some("primary".into()),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    fn static_authority(api_key: &str) -> ContinuationAuthority {
        ContinuationAuthority::new(
            CredentialAuthority::derive("static-transport-credential", api_key),
            bitrouter_sdk::language_model::types::AuthScheme::Bearer,
        )
    }

    #[test]
    fn resolved_continuation_debug_redacts_native_and_fingerprint_values() {
        let resolved = ResolvedContinuation {
            provider_response_id: "provider-debug-private-sentinel".into(),
            effective_model: Some("provider:model".into()),
            effective_effort: None,
            effort_authoritative: true,
            causal_prefix_commitment: None,
            target_fingerprint: "fingerprint-debug-private-sentinel".into(),
            key: ContinuationKey::from_bytes([74; 32]).expect("continuation key"),
        };
        let debug = format!("{resolved:?}");

        for private in [
            "provider-debug-private-sentinel",
            "fingerprint-debug-private-sentinel",
        ] {
            assert!(
                !debug.contains(private),
                "ResolvedContinuation Debug exposed private capability data: {debug}"
            );
        }
        assert!(debug.contains("<redacted>"));
    }

    async fn registry(secret: u8) -> anyhow::Result<ContinuationRegistry> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        ContinuationRegistry::new(
            db,
            ContinuationKeySource::fixed(ContinuationKey::from_bytes([secret; 32])?),
            30,
            10,
        )
    }

    async fn independent_file_registries(
        secret: u8,
    ) -> anyhow::Result<(
        tempfile::TempDir,
        ContinuationRegistry,
        ContinuationRegistry,
    )> {
        let directory = tempfile::tempdir()?;
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("continuations.db").display()
        );
        let first_db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&first_db).await?;
        let second_db = crate::db::connect(&database_url).await?;
        let keys = ContinuationKeySource::fixed(ContinuationKey::from_bytes([secret; 32])?);
        let first = ContinuationRegistry::new(first_db, keys.clone(), 30, 10)?;
        let second = ContinuationRegistry::new(second_db, keys, 30, 10)?;
        Ok((directory, first, second))
    }

    fn app_state(pipeline: Arc<Pipeline>) -> AppState {
        AppState {
            language_model: pipeline,
            mcp: None,
            skip_auth: true,
            metrics_renderer: None,
            prompt_transforms: Vec::new(),
        }
    }

    #[test]
    fn continuation_payload_debug_redacts_native_id_and_commitment() -> anyhow::Result<()> {
        let commitment = CausalPrefixCommitment::parse(&format!("sha256:{}", "ab".repeat(32)))
            .ok_or_else(|| anyhow::anyhow!("test commitment missing"))?;
        let payload = ContinuationPayload::current(
            "provider-debug-private-sentinel",
            "openai:gpt-5",
            None,
            Some(&commitment),
        )?;
        let debug = format!("{payload:?}");

        assert!(!debug.contains("provider-debug-private-sentinel"));
        assert!(!debug.contains(commitment.as_str()));
        assert!(debug.contains("openai:gpt-5"));
        Ok(())
    }

    fn responses_http_request(request_id: &str, stream: bool) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .header("x-bitrouter-request-id", request_id)
            .body(Body::from(
                serde_json::json!({
                    "model": "gpt-5",
                    "input": "say ready",
                    "stream": stream,
                })
                .to_string(),
            ))
            .expect("valid Responses request")
    }

    fn valid_nonstream_result(target: &RoutingTarget, response_id: &str) -> ExecutionResult {
        ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: GenerateResult {
                content: vec![Content::Text {
                    text: "ready".into(),
                    provider_metadata: Default::default(),
                }],
                usage: Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 11,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
                response_id: Some(response_id.into()),
                stop_details: None,
                provider_metadata: Default::default(),
            },
            request_duration_ms: 1,
            upstream_duration_ms: Some(1),
            server_tool_calls: Vec::new(),
        }
    }

    fn pending_publication_count(registry: &ContinuationRegistry) -> usize {
        match registry.inner.pending_publications.lock() {
            Ok(publications) => publications.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    async fn wait_for_pending_publication_count(
        registry: &ContinuationRegistry,
        expected: usize,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if pending_publication_count(registry) == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for {expected} pending publications; observed {}",
                pending_publication_count(registry)
            )
        })?;
        Ok(())
    }

    fn reconciler_task_id(registry: &ContinuationRegistry) -> Option<tokio::task::Id> {
        let task = match registry.inner.reconciler.task.lock() {
            Ok(task) => task,
            Err(poisoned) => poisoned.into_inner(),
        };
        task.as_ref().map(|task| task.handle.id())
    }

    async fn suspend_reconciler_for_test(registry: &ContinuationRegistry) {
        let task = {
            let mut task = match registry.inner.reconciler.task.lock() {
                Ok(task) => task,
                Err(poisoned) => poisoned.into_inner(),
            };
            task.take()
        };
        if let Some(mut task) = task {
            task.cancelled.store(true, Ordering::Release);
            registry.inner.reconciler.notify.notify_waiters();
            if tokio::time::timeout(Duration::from_secs(1), &mut task.handle)
                .await
                .is_err()
            {
                task.handle.abort();
            }
        }
    }

    struct GatedResponsesExecutor {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Executor for GatedResponsesExecutor {
        async fn execute(
            &self,
            target: &RoutingTarget,
            _prompt: &Prompt,
            _ctx: &PipelineContext,
        ) -> PipelineResult<ExecutionResult> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(valid_nonstream_result(target, "provider-detached-final"))
        }

        async fn execute_stream(
            &self,
            _target: &RoutingTarget,
            _prompt: &Prompt,
            _ctx: &PipelineContext,
        ) -> PipelineResult<StreamPartStream> {
            Err(BitrouterError::internal("streaming not used"))
        }
    }

    #[tokio::test]
    async fn server_nonstream_handler_drop_settles_billing_without_publishing_continuation()
    -> anyhow::Result<()> {
        let registry = registry(43).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let usage = Arc::new(std::sync::Mutex::new(Vec::new()));
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(GatedResponsesExecutor {
                started: started.clone(),
                release: release.clone(),
            }))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(UsageSettlementRecorder(usage.clone()))
            .observe_hook(OutcomeObserver(outcomes.clone()));
        let pipeline = Arc::new(builder.build()?);

        let mut handler = Box::pin(
            build_router(app_state(pipeline.clone()))
                .oneshot(responses_http_request("server-detached-drop", false)),
        );
        assert!(
            futures::poll!(handler.as_mut()).is_pending(),
            "real HTTP handler must still be waiting on the gated upstream"
        );
        started.notified().await;
        drop(handler);
        release.notify_one();
        pipeline.drain_pending_settlements().await;

        assert_eq!(
            usage.lock().expect("usage settlement poisoned").as_slice(),
            &[(7, 11)],
            "detached execution must retain full upstream billing"
        );
        assert_eq!(
            registry
                .resolve(
                    CallerContext::local().user_id(),
                    &encode_gateway_continuation_id("server-detached-drop")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "a detached billing completion must not publish a resumable handle"
        );
        assert_eq!(
            outcomes
                .lock()
                .expect("outcome observer poisoned")
                .as_slice(),
            &["disconnected"],
            "the dropped handler is not a delivered success"
        );
        Ok(())
    }

    #[tokio::test]
    async fn encrypted_registry_round_trips_without_plaintext() -> anyhow::Result<()> {
        let registry = registry(7).await?;
        let now = Utc
            .with_ymd_and_hms(2026, 8, 2, 0, 0, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("test timestamp is invalid"))?;
        registry
            .bind(
                "owner-a",
                "gateway-request-a",
                "resp-provider-secret",
                &target("credential-a"),
                &static_authority("credential-a"),
                now,
            )
            .await?;

        let resolved = registry
            .resolve("owner-a", "gateway-request-a", now)
            .await?;
        let ContinuationResolution::Active(active) = resolved else {
            anyhow::bail!("expected active continuation")
        };
        assert_eq!(active.provider_response_id, "resp-provider-secret");
        assert_eq!(active.effective_model.as_deref(), Some("openai:gpt-5"));
        assert!(active.matches_target(&target("credential-a"), &static_authority("credential-a"))?);
        assert!(
            !active.matches_target(&target("credential-b"), &static_authority("credential-b"))?
        );
        assert_eq!(
            registry
                .resolve("owner-b", "gateway-request-a", now)
                .await?,
            ContinuationResolution::Missing
        );

        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT * FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation row missing"))?;
        let durable = [
            row.try_get::<String>("", "owner_identity")?,
            row.try_get::<String>("", "continuation_identity")?,
            row.try_get::<String>("", "ciphertext")?,
            row.try_get::<String>("", "nonce")?,
            row.try_get::<String>("", "target_fingerprint")?,
        ]
        .join("|");
        for plaintext in [
            "owner-a",
            "gateway-request-a",
            "resp-provider-secret",
            "credential-a",
            "api.openai.example",
        ] {
            assert!(!durable.contains(plaintext), "leaked {plaintext}");
        }
        let sealed = continuation_entity::Entity::find()
            .one(registry.database())
            .await?
            .ok_or_else(|| anyhow::anyhow!("sealed continuation row missing"))?;
        let envelope = decrypt_row(&ContinuationKey::from_bytes([7; 32])?, &sealed)?;
        assert!(envelope.starts_with(CONTINUATION_PAYLOAD_V4_MAGIC));
        assert!(
            String::from_utf8(envelope).is_err(),
            "v4 envelope must not collide with any legacy UTF-8 response id"
        );

        let reloaded = ContinuationRegistry::new(
            registry.database().clone(),
            ContinuationKeySource::fixed(ContinuationKey::from_bytes([7; 32])?),
            30,
            10,
        )?;
        let ContinuationResolution::Active(reloaded) = reloaded
            .resolve("owner-a", "gateway-request-a", now)
            .await?
        else {
            anyhow::bail!("reloaded registry lost the continuation")
        };
        assert_eq!(reloaded.provider_response_id, "resp-provider-secret");
        assert_eq!(reloaded.effective_model.as_deref(), Some("openai:gpt-5"));
        Ok(())
    }

    #[tokio::test]
    async fn v1_magic_prefix_collision_decodes_as_unservable_legacy() -> anyhow::Result<()> {
        let registry = registry(77).await?;
        let gateway_id = encode_gateway_continuation_id("v1-gateway")?;
        let now = Utc::now();
        registry
            .bind(
                "v1-owner",
                &gateway_id,
                "v1-provider-response",
                &target("v1-credential"),
                &static_authority("v1-credential"),
                now,
            )
            .await?;
        let key = ContinuationKey::from_bytes([77; 32])?;
        let identity = key.continuation_identity("v1-owner", &gateway_id)?;
        let row = continuation_entity::Entity::find_by_id(&identity)
            .one(registry.database())
            .await?
            .ok_or_else(|| anyhow::anyhow!("v1 continuation row missing"))?;
        let aad = aead_aad([
            &row.key_id,
            &row.owner_identity,
            &row.continuation_identity,
            &row.target_fingerprint,
            &row.created_at,
            &row.expires_at,
            &row.purge_after,
            &row.publication_state,
            &row.publication_generation,
            &row.publication_instance_id,
            &row.publication_lease_until,
        ]);
        let v1 = format!(
            "{CONTINUATION_PAYLOAD_V1_PREFIX}{}",
            serde_json::json!({
                "version": 1,
                "provider_response_id": "v1-provider-response",
                "effective_model": "openai:gpt-5"
            })
        );
        let (ciphertext, nonce) = encrypt(&key, v1.as_bytes(), &aad)?;
        let mut v1_row: continuation_entity::ActiveModel = row.into();
        v1_row.ciphertext = Set(Some(ciphertext));
        v1_row.nonce = Set(Some(nonce));
        v1_row.update(registry.database()).await?;

        let ContinuationResolution::Active(resolved) =
            registry.resolve("v1-owner", &gateway_id, now).await?
        else {
            anyhow::bail!("v1 continuation did not resolve")
        };
        assert_eq!(resolved.provider_response_id, v1);
        assert_eq!(resolved.effective_model, None);
        assert_eq!(resolved.causal_prefix_commitment, None);

        let runtime = ContinuationRuntime::new(registry);
        let mut ctx = continuation_context_for_owner(&gateway_id, "v1-owner");
        PreRequestHook::check(&runtime, &mut ctx).await?;
        assert!(
            ctx.extension::<ContinuationRequestPlan>()
                .is_some_and(|plan| {
                    matches!(
                        plan.adjustment.as_ref(),
                        Some(ContinuationAdjustment::RejectLegacy)
                    )
                })
        );
        assert!(ctx.extension::<SuppressProviderContinuation>().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn current_payload_without_commitment_pins_visible_history() -> anyhow::Result<()> {
        let registry = registry(78).await?;
        let gateway_id = encode_gateway_continuation_id("current-without-commitment")?;
        registry
            .bind(
                "current-owner",
                &gateway_id,
                "current-provider-response",
                &target("current-credential"),
                &static_authority("current-credential"),
                Utc::now(),
            )
            .await?;
        let runtime = ContinuationRuntime::new(registry);
        let mut ctx = continuation_context_for_owner_with_messages(
            &gateway_id,
            "current-owner",
            vec![
                Message::text(Role::User, "Design the architecture"),
                Message::text(Role::Assistant, "untrusted visible parent"),
                Message::text(Role::User, "Implement it"),
            ],
        );

        PreRequestHook::check(&runtime, &mut ctx).await?;

        assert!(
            ctx.extension::<ContinuationRequestPlan>()
                .is_some_and(|plan| matches!(
                    plan.adjustment,
                    Some(ContinuationAdjustment::Pin { .. })
                ))
        );
        assert!(ctx.extension::<SuppressProviderContinuation>().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn legacy_ciphertext_resolves_without_guessing_an_effective_model() -> anyhow::Result<()>
    {
        let registry = registry(75).await?;
        let gateway_id = encode_gateway_continuation_id("legacy-gateway")?;
        let now = Utc::now();
        registry
            .bind(
                "legacy-owner",
                &gateway_id,
                "legacy-provider-response",
                &target("legacy-credential"),
                &static_authority("legacy-credential"),
                now,
            )
            .await?;
        let key = ContinuationKey::from_bytes([75; 32])?;
        let continuation_identity = key.continuation_identity("legacy-owner", &gateway_id)?;
        let row = continuation_entity::Entity::find_by_id(&continuation_identity)
            .one(registry.database())
            .await?
            .ok_or_else(|| anyhow::anyhow!("legacy continuation row missing"))?;
        let aad = aead_aad([
            &row.key_id,
            &row.owner_identity,
            &row.continuation_identity,
            &row.target_fingerprint,
            &row.created_at,
            &row.expires_at,
            &row.purge_after,
            &row.publication_state,
            &row.publication_generation,
            &row.publication_instance_id,
            &row.publication_lease_until,
        ]);
        let (ciphertext, nonce) = encrypt(&key, b"legacy-provider-response", &aad)?;
        let mut legacy: continuation_entity::ActiveModel = row.into();
        legacy.ciphertext = Set(Some(ciphertext));
        legacy.nonce = Set(Some(nonce));
        legacy.update(registry.database()).await?;

        let ContinuationResolution::Active(resolved) =
            registry.resolve("legacy-owner", &gateway_id, now).await?
        else {
            anyhow::bail!("legacy continuation did not resolve")
        };
        assert_eq!(resolved.provider_response_id, "legacy-provider-response");
        assert_eq!(resolved.effective_model, None);

        let runtime = ContinuationRuntime::new(registry);
        let mut cross_provider = target("other-credential");
        cross_provider.provider_name = "other".into();
        for candidate in [target("legacy-credential"), cross_provider] {
            let error = match runtime
                .resolve(
                    &mut vec![candidate],
                    &mut continuation_context_for_owner(&gateway_id, "legacy-owner"),
                )
                .await
            {
                Ok(()) => anyhow::bail!("legacy plaintext reached an upstream route"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("model provenance"),
                "unexpected continuation error: {error}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn registry_is_idempotent_but_rejects_collisions_and_rotation() -> anyhow::Result<()> {
        let registry = registry(8).await?;
        let now = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
        let bound_target = target("credential");
        let authority = static_authority("credential");
        let bind = || {
            registry.bind(
                "owner",
                "gateway",
                "resp-same",
                &bound_target,
                &authority,
                now,
            )
        };
        let (left, right) = tokio::join!(bind(), bind());
        left?;
        right?;
        let collision = registry
            .bind(
                "owner",
                "gateway",
                "resp-different",
                &target("credential"),
                &static_authority("credential"),
                now,
            )
            .await
            .unwrap_err();
        assert!(collision.to_string().contains("already bound"));

        let rotated = ContinuationRegistry::new(
            registry.database().clone(),
            ContinuationKeySource::fixed(ContinuationKey::from_bytes([9; 32])?),
            30,
            10,
        )?;
        let error = rotated.resolve("owner", "gateway", now).await.unwrap_err();
        assert!(error.to_string().contains("key epoch"));
        Ok(())
    }

    #[tokio::test]
    async fn registry_tamper_expiry_and_pruning_fail_closed() -> anyhow::Result<()> {
        let tamper_registry = registry(10).await?;
        let now = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
        tamper_registry
            .bind(
                "owner",
                "gateway-tamper",
                "resp-secret",
                &target("credential"),
                &static_authority("credential"),
                now,
            )
            .await?;
        tamper_registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE provider_continuations SET ciphertext = 'AAAA'".to_owned(),
            ))
            .await?;
        let tampered = tamper_registry
            .resolve("owner", "gateway-tamper", now)
            .await
            .unwrap_err();
        assert!(tampered.to_string().contains("authentication"));

        let expiry_registry = registry(11).await?;
        expiry_registry
            .bind(
                "owner",
                "gateway-expiry",
                "resp-expiry",
                &target("credential"),
                &static_authority("credential"),
                now,
            )
            .await?;
        let expired_at = now + TimeDelta::try_days(31).unwrap();
        assert_eq!(
            expiry_registry
                .resolve("owner", "gateway-expiry", expired_at)
                .await?,
            ContinuationResolution::Expired
        );
        let scrubbed = expiry_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT ciphertext, nonce FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("expired tombstone missing"))?;
        assert!(
            scrubbed
                .try_get::<Option<String>>("", "ciphertext")?
                .is_none()
        );
        assert!(scrubbed.try_get::<Option<String>>("", "nonce")?.is_none());

        let purge_at = now + TimeDelta::try_days(61).unwrap();
        assert_eq!(expiry_registry.prune(purge_at).await?, 1);
        assert_eq!(
            expiry_registry
                .resolve("owner", "gateway-expiry", purge_at)
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_scrub_never_redacts_a_rebound_generation() -> anyhow::Result<()> {
        let (_directory, stale_registry, rebinding_registry) =
            independent_file_registries(59).await?;
        let created_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
        let maintenance_at = created_at + TimeDelta::try_days(31).unwrap();
        stale_registry
            .bind(
                "owner",
                "scrub-race",
                "provider-old",
                &target("credential"),
                &static_authority("credential"),
                created_at,
            )
            .await?;
        let (snapshot_read, release) =
            stale_registry.pause_maintenance_after_snapshot(MaintenanceFaultKind::Scrub);
        let resolving_registry = stale_registry.clone();
        let resolve = tokio::spawn(async move {
            resolving_registry
                .resolve("owner", "scrub-race", maintenance_at)
                .await
        });
        snapshot_read.await?;

        let continuation_identity = rebinding_registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", "scrub-race")?;
        continuation_entity::Entity::delete_by_id(continuation_identity)
            .exec(rebinding_registry.database())
            .await?;
        rebinding_registry
            .bind(
                "owner",
                "scrub-race",
                "provider-new",
                &target("credential"),
                &static_authority("credential"),
                maintenance_at,
            )
            .await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("scrub release closed"))?;

        let resolved = resolve
            .await
            .expect("resolve task joins")
            .expect("stale scrub must not redact the rebound generation");
        let ContinuationResolution::Active(resolved) = resolved else {
            panic!("stale scrub must reload the rebound active generation")
        };
        assert_eq!(resolved.provider_response_id, "provider-new");
        Ok(())
    }

    #[tokio::test]
    async fn batch_scrub_never_redacts_a_rebound_generation() -> anyhow::Result<()> {
        let (_directory, stale_registry, rebinding_registry) =
            independent_file_registries(60).await?;
        let created_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
        let maintenance_at = created_at + TimeDelta::try_days(31).unwrap();
        stale_registry
            .bind(
                "owner",
                "batch-scrub-race",
                "provider-old",
                &target("credential"),
                &static_authority("credential"),
                created_at,
            )
            .await?;
        let (snapshot_read, release) =
            stale_registry.pause_maintenance_after_snapshot(MaintenanceFaultKind::Scrub);
        let pruning_registry = stale_registry.clone();
        let prune = tokio::spawn(async move { pruning_registry.prune(maintenance_at).await });
        snapshot_read.await?;

        let continuation_identity = rebinding_registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", "batch-scrub-race")?;
        continuation_entity::Entity::delete_by_id(continuation_identity)
            .exec(rebinding_registry.database())
            .await?;
        rebinding_registry
            .bind(
                "owner",
                "batch-scrub-race",
                "provider-new",
                &target("credential"),
                &static_authority("credential"),
                maintenance_at,
            )
            .await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("batch scrub release closed"))?;
        assert_eq!(prune.await??, 0);

        let resolved = rebinding_registry
            .resolve("owner", "batch-scrub-race", maintenance_at)
            .await
            .expect("batch scrub must not redact the rebound generation");
        let ContinuationResolution::Active(resolved) = resolved else {
            panic!("batch scrub redacted the rebound active generation")
        };
        assert_eq!(resolved.provider_response_id, "provider-new");
        Ok(())
    }

    #[tokio::test]
    async fn purge_never_deletes_a_rebound_generation() -> anyhow::Result<()> {
        let (_directory, stale_registry, rebinding_registry) =
            independent_file_registries(61).await?;
        let created_at = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
        let maintenance_at = created_at + TimeDelta::try_days(61).unwrap();
        stale_registry
            .bind(
                "owner",
                "purge-race",
                "provider-old",
                &target("credential"),
                &static_authority("credential"),
                created_at,
            )
            .await?;
        let (snapshot_read, release) =
            stale_registry.pause_maintenance_after_snapshot(MaintenanceFaultKind::Purge);
        let pruning_registry = stale_registry.clone();
        let prune = tokio::spawn(async move { pruning_registry.prune(maintenance_at).await });
        snapshot_read.await?;

        let continuation_identity = rebinding_registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", "purge-race")?;
        continuation_entity::Entity::delete_by_id(continuation_identity)
            .exec(rebinding_registry.database())
            .await?;
        rebinding_registry
            .bind(
                "owner",
                "purge-race",
                "provider-new",
                &target("credential"),
                &static_authority("credential"),
                maintenance_at,
            )
            .await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("purge release closed"))?;
        assert_eq!(
            prune.await??,
            0,
            "a stale purge snapshot must report no deleted row"
        );

        let resolved = rebinding_registry
            .resolve("owner", "purge-race", maintenance_at)
            .await?;
        let ContinuationResolution::Active(resolved) = resolved else {
            panic!("stale purge deleted the rebound active generation")
        };
        assert_eq!(resolved.provider_response_id, "provider-new");
        Ok(())
    }

    #[tokio::test]
    async fn publication_state_tamper_cannot_turn_provisional_into_resolvable_active()
    -> anyhow::Result<()> {
        let registry = registry(49).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("state-tamper", 407, "provider-state-secret");
        runtime.finalize(&attempt).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE provider_continuations SET publication_state = 'active'".to_owned(),
            ))
            .await?;

        let independent = ContinuationRegistry::new(
            registry.inner.db.clone(),
            registry.inner.keys.clone(),
            30,
            10,
        )?;
        let error = independent
            .resolve(
                "owner",
                &encode_gateway_continuation_id("state-tamper")?,
                Utc::now(),
            )
            .await
            .expect_err("changing publication state without re-sealing must fail authentication");
        assert!(error.to_string().contains("authentication"));
        Ok(())
    }

    #[tokio::test]
    async fn publication_generation_is_random_and_authenticated() -> anyhow::Result<()> {
        let registry = registry(50).await?;
        let now = Utc::now();
        for (gateway_id, provider_id) in [
            ("generation-a", "provider-generation-a"),
            ("generation-b", "provider-generation-b"),
        ] {
            registry
                .bind(
                    "owner",
                    gateway_id,
                    provider_id,
                    &target("credential"),
                    &static_authority("credential"),
                    now,
                )
                .await?;
        }
        let rows = registry
            .database()
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT continuation_identity, publication_generation FROM provider_continuations ORDER BY continuation_identity".to_owned(),
            ))
            .await?;
        let generations = rows
            .iter()
            .map(|row| row.try_get::<String>("", "publication_generation"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(generations.len(), 2);
        assert!(generations.iter().all(|generation| !generation.is_empty()));
        assert_ne!(generations[0], generations[1]);

        registry
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE provider_continuations SET publication_generation = ? WHERE continuation_identity = ?",
                [
                    generations[1].clone().into(),
                    registry
                        .inner
                        .keys
                        .load()?
                        .continuation_identity("owner", "generation-a")?
                        .into(),
                ],
            ))
            .await?;
        let error = registry
            .resolve("owner", "generation-a", now)
            .await
            .expect_err("changing a generation token without re-sealing must fail authentication");
        assert!(error.to_string().contains("authentication"));
        Ok(())
    }

    #[tokio::test]
    async fn publication_instance_and_lease_are_authenticated() -> anyhow::Result<()> {
        let registry = registry(83).await?;
        let now = Utc::now();
        for gateway_id in ["instance-tamper", "lease-tamper"] {
            registry
                .bind(
                    "owner",
                    gateway_id,
                    &format!("provider-{gateway_id}"),
                    &target("credential"),
                    &static_authority("credential"),
                    now,
                )
                .await?;
        }
        let instance_identity = registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", "instance-tamper")?;
        let lease_identity = registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", "lease-tamper")?;
        registry
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE provider_continuations SET publication_instance_id = ? WHERE continuation_identity = ?",
                ["foreign-instance".into(), instance_identity.into()],
            ))
            .await?;
        registry
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE provider_continuations SET publication_lease_until = ? WHERE continuation_identity = ?",
                [timestamp(now + TimeDelta::seconds(300)).into(), lease_identity.into()],
            ))
            .await?;

        for gateway_id in ["instance-tamper", "lease-tamper"] {
            let error = registry.resolve("owner", gateway_id, now).await.expect_err(
                "changing publication fencing without re-sealing must fail authentication",
            );
            assert!(error.to_string().contains("authentication"));
        }
        Ok(())
    }

    fn continuation_context(previous_response_id: &str) -> PipelineContext {
        continuation_context_for_owner(previous_response_id, "owner")
    }

    fn continuation_context_for_owner(previous_response_id: &str, owner: &str) -> PipelineContext {
        continuation_context_for_owner_with_messages(
            previous_response_id,
            owner,
            vec![Message::text(Role::User, "continue")],
        )
    }

    fn continuation_context_for_owner_with_messages(
        previous_response_id: &str,
        owner: &str,
        messages: Vec<Message>,
    ) -> PipelineContext {
        let mut params = GenerationParams::default();
        params.extra.insert(
            "previous_response_id".into(),
            serde_json::Value::String(previous_response_id.into()),
        );
        PipelineContext::new(PipelineRequest {
            request_id: "next-gateway".into(),
            model: "openai:gpt-5".into(),
            caller: CallerContext::new("key", owner),
            headers: Default::default(),
            prompt: Prompt {
                model: "gpt-5".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages,
                tools: Vec::new(),
                params,
                response_format: None,
                tool_choice: None,
                stream: true,
            },
            inbound_protocol: Some(ApiProtocol::Responses),
        })
    }

    #[tokio::test]
    async fn mapped_route_pins_selected_authority_and_installs_native_override()
    -> anyhow::Result<()> {
        let registry = registry(12).await?;
        let now = Utc::now();
        let bound = target("credential-a");
        let public_id = encode_gateway_continuation_id("prior-request")?;
        registry
            .bind(
                "owner",
                &public_id,
                "resp-final",
                &bound,
                &static_authority("credential-a"),
                now,
            )
            .await?;
        let runtime = ContinuationRuntime::new(registry);
        let mut selected_tier = bound.clone();
        selected_tier.service_id = "gpt-5-strong".into();
        let mut chain = vec![selected_tier.clone()];
        let mut ctx = continuation_context(&public_id);

        runtime.resolve(&mut chain, &mut ctx).await?;

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].service_id, selected_tier.service_id);
        assert!(ctx.extension::<ProviderContinuation>().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn changed_authority_is_unavailable_and_fails_closed() -> anyhow::Result<()> {
        let registry = registry(15).await?;
        let bound = target("credential-a");
        let public_id = encode_gateway_continuation_id("prior-request")?;
        registry
            .bind(
                "owner",
                &public_id,
                "resp-final",
                &bound,
                &static_authority("credential-a"),
                Utc::now(),
            )
            .await?;
        let runtime = ContinuationRuntime::new(registry);

        let mut endpoint_changed = bound.clone();
        endpoint_changed.api_base = "https://changed.example/v1".into();
        let mut credential_changed = bound.clone();
        credential_changed.api_key = "credential-b".into();
        let mut account_changed = bound.clone();
        account_changed.account_label = Some("secondary".into());
        let mut provider_changed = bound.clone();
        provider_changed.provider_name = "other-provider".into();
        for changed in [
            endpoint_changed,
            credential_changed,
            account_changed,
            provider_changed,
        ] {
            let mut chain = vec![changed];
            let error = runtime
                .resolve(&mut chain, &mut continuation_context(&public_id))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("unavailable or changed"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn active_continuation_skips_broken_non_responses_authority() -> anyhow::Result<()> {
        let registry = registry(24).await?;
        let bound = target("credential-a");
        let public_id = encode_gateway_continuation_id("mixed-prior-request")?;
        registry
            .bind(
                "owner",
                &public_id,
                "resp-final",
                &bound,
                &static_authority("credential-a"),
                Utc::now(),
            )
            .await?;
        let auth =
            AuthAppliers::new().with("broken-messages", Arc::new(FailingAuthorityAuthApplier));
        let runtime = ContinuationRuntime::with_auth_appliers(registry, auth);
        let mut messages = bound.clone();
        messages.provider_name = "broken-messages".into();
        messages.api_protocol = ApiProtocol::Messages;
        let mut chain = vec![messages, bound.clone()];
        let mut ctx = continuation_context(&public_id);

        runtime.resolve(&mut chain, &mut ctx).await?;

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, bound.provider_name);
        assert_eq!(chain[0].service_id, bound.service_id);
        assert!(ctx.extension::<ProviderContinuation>().is_some());
        Ok(())
    }

    #[tokio::test]
    async fn mapped_continuation_replaces_opaque_authority_failure_text() -> anyhow::Result<()> {
        const SENTINEL: &str = "opaque-authority-private-sentinel";
        let registry = registry(72).await?;
        let bound = target("credential-a");
        let public_id = encode_gateway_continuation_id("opaque-authority-prior-request")?;
        registry
            .bind(
                "owner",
                &public_id,
                "resp-final",
                &bound,
                &static_authority("credential-a"),
                Utc::now(),
            )
            .await?;
        let auth = AuthAppliers::new().with(
            bound.provider_name.clone(),
            Arc::new(SecretFailingAuthorityAuthApplier),
        );
        let runtime = ContinuationRuntime::with_auth_appliers(registry, auth);
        let mut chain = vec![bound];
        let error = runtime
            .resolve(&mut chain, &mut continuation_context(&public_id))
            .await
            .expect_err("opaque authority failure must fail mapped continuation routing");
        let diagnostic = error.to_string();

        assert!(
            !diagnostic.contains(SENTINEL),
            "opaque continuation authority detail reached the route caller: {diagnostic}"
        );
        assert!(diagnostic.contains("continuation authentication authority resolution failed"));
        Ok(())
    }

    #[tokio::test]
    async fn public_responses_and_runtime_diagnostics_replace_opaque_apply_failure_text()
    -> anyhow::Result<()> {
        const SENTINEL: &str = "opaque-public-auth-private-sentinel";
        let upstream = MockServer::start().await;
        let registry = registry(73).await?;
        let auth = AuthAppliers::new().with("openai", Arc::new(SecretFailingApplyAuthApplier));
        let runtime = ContinuationRuntime::with_auth_appliers(registry, auth.clone());
        let mut upstream_target = target("static-placeholder");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let settlement = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor =
            HttpExecutor::with_dispatch_and_auth(Default::default(), Default::default(), auth)?;
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(executor))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(NativeIdStealingRecorder(settlement.clone()));
        let pipeline = Arc::new(builder.build()?);
        let app = build_router(app_state(pipeline.clone()));

        let captured_logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_sink = captured_logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || CapturedLogWriter(log_sink.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let mut public_bodies = Vec::new();
        for (request_id, stream) in [
            ("opaque-auth-public-nonstream", false),
            ("opaque-auth-public-stream", true),
        ] {
            let response = app
                .clone()
                .oneshot(responses_http_request(request_id, stream))
                .await?;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            );
            public_bodies.push(String::from_utf8(
                axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await?
                    .to_vec(),
            )?);
        }
        pipeline.drain_pending_settlements().await;

        let diagnostics = [
            public_bodies.join("\n"),
            settlement
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .join("\n"),
            String::from_utf8(
                captured_logs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )?,
        ];
        for diagnostic in diagnostics {
            assert!(
                !diagnostic.contains(SENTINEL),
                "opaque authentication detail reached a public/runtime surface: {diagnostic}"
            );
        }
        assert!(
            public_bodies
                .iter()
                .all(|body| body.contains("upstream authentication failed"))
        );
        assert_eq!(
            upstream
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("request recording disabled"))?
                .len(),
            0,
            "opaque authentication failure must stop before provider dispatch"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_gateway_fails_closed_but_native_compatibility_is_pinned() -> anyhow::Result<()>
    {
        let runtime = ContinuationRuntime::new(registry(13).await?);
        let mut missing_chain = vec![target("first"), target("fallback")];
        let missing_public = encode_gateway_continuation_id("missing-request")?;
        let error = runtime
            .resolve(
                &mut missing_chain,
                &mut continuation_context(&missing_public),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mapping is unavailable"));

        let mut native_chain = vec![target("first"), target("fallback")];
        let mut native_ctx = continuation_context("provider-native-arbitrary-format");
        runtime.resolve(&mut native_chain, &mut native_ctx).await?;
        assert_eq!(native_chain.len(), 1);
        assert!(native_ctx.extension::<ProviderContinuation>().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn native_continuation_does_not_guess_an_auto_routed_provider() -> anyhow::Result<()> {
        let runtime = ContinuationRuntime::new(registry(76).await?);
        let mut auto = continuation_context("provider-native-arbitrary-format");
        auto.set_model("@auto");

        let decision = PreRequestHook::check(&runtime, &mut auto).await?;

        assert!(matches!(decision, HookDecision::Allow));
        assert!(auto.extension::<RejectContinuationPreflight>().is_some());
        let error = RouteHook::resolve(&runtime, &mut vec![target("first")], &mut auto)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("explicit provider model"));

        let mut explicit = continuation_context("provider-native-arbitrary-format");
        assert!(matches!(
            PreRequestHook::check(&runtime, &mut explicit).await?,
            HookDecision::Allow
        ));
        Ok(())
    }

    #[tokio::test]
    async fn required_finalizer_publishes_native_mapping() -> anyhow::Result<()> {
        let registry = registry(14).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut finalization = finalization_context("gateway-result", 1, "resp-provider-final");
        finalization.causal_prefix_commitment =
            CausalPrefixCommitment::parse(&format!("sha256:{}", "cd".repeat(32)));
        runtime.finalize(&finalization).await?;
        commit_delivered(&runtime, &finalization).await?;
        let resolved = registry
            .resolve(
                "owner",
                &encode_gateway_continuation_id("gateway-result")?,
                Utc::now(),
            )
            .await?;
        let ContinuationResolution::Active(resolved) = resolved else {
            anyhow::bail!("required finalizer did not publish an active mapping")
        };
        assert!(resolved.causal_prefix_commitment.is_some());
        Ok(())
    }

    fn finalization_context(
        request_id: &str,
        delivery_attempt_id: u64,
        provider_response_id: &str,
    ) -> RequiredFinalizationContext {
        RequiredFinalizationContext {
            request_id: request_id.into(),
            delivery_attempt_id,
            caller: CallerContext::new("key", "owner"),
            target: Some(target("credential")),
            effective_model: "openai:gpt-5".into(),
            effective_effort: None,
            causal_prefix_commitment: None,
            inbound_protocol: Some(ApiProtocol::Responses),
            response_id: Some(provider_response_id.into()),
            finish_reason: Some(bitrouter_sdk::language_model::FinishReason::Stop),
            streamed: true,
            successful_terminal: true,
            native_response_completed: true,
            credential_authority: Some(static_authority("credential")),
        }
    }

    async fn commit_delivered(
        finalizer: &dyn RequiredFinalizer,
        ctx: &RequiredFinalizationContext,
    ) -> PipelineResult<()> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new(ready_tx, ack_rx);
        let acknowledge = tokio::spawn(async move {
            ready_rx
                .await
                .map_err(|error| BitrouterError::internal(error.to_string()))??;
            ack_tx
                .send(DeliveryAcknowledgement::Delivered)
                .map_err(|_| BitrouterError::internal("delivery acknowledgement closed"))
        });
        assert!(finalizer.commit(ctx, &delivery).await?);
        acknowledge
            .await
            .map_err(|error| BitrouterError::internal(error.to_string()))??;
        Ok(())
    }

    async fn commit_disconnected(
        finalizer: &dyn RequiredFinalizer,
        ctx: &RequiredFinalizationContext,
    ) -> PipelineResult<bool> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new(ready_tx, ack_rx);
        let disconnect = tokio::spawn(async move {
            match ready_rx.await {
                Ok(Ok(())) | Ok(Err(_)) | Err(_) => drop(ack_tx),
            }
        });
        let result = finalizer.commit(ctx, &delivery).await;
        disconnect
            .await
            .map_err(|error| BitrouterError::internal(error.to_string()))?;
        result
    }

    #[tokio::test]
    async fn rollback_of_idempotent_active_mapping_never_deletes_existing_row() -> anyhow::Result<()>
    {
        let registry = registry(39).await?;
        let public_id = encode_gateway_continuation_id("existing-active")?;
        registry
            .bind(
                "owner",
                &public_id,
                "provider-existing",
                &target("credential"),
                &static_authority("credential"),
                Utc::now(),
            )
            .await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("existing-active", 101, "provider-existing");
        runtime.finalize(&attempt).await?;
        runtime.rollback(&attempt).await?;
        assert!(matches!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn second_concurrent_attempt_fails_without_disturbing_first_publication()
    -> anyhow::Result<()> {
        let registry = registry(40).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let first = finalization_context("shared-request", 201, "provider-shared");
        let second = finalization_context("shared-request", 202, "provider-shared");
        runtime.finalize(&first).await?;
        runtime
            .finalize(&second)
            .await
            .expect_err("a second attempt must not adopt the first provisional generation");
        let public_id = encode_gateway_continuation_id("shared-request")?;
        assert_eq!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Missing
        );

        runtime.rollback(&second).await?;
        commit_delivered(&runtime, &first).await?;
        runtime.rollback(&second).await?;
        assert!(matches!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn all_concurrent_attempts_cancel_delete_only_their_new_row() -> anyhow::Result<()> {
        let registry = registry(43).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let first = finalization_context("all-cancelled", 211, "provider-cancelled");
        let second = finalization_context("all-cancelled", 212, "provider-cancelled");
        runtime.finalize(&first).await?;
        runtime
            .finalize(&second)
            .await
            .expect_err("a second attempt must fail closed while the first is provisional");
        runtime.rollback(&second).await?;
        runtime.rollback(&first).await?;

        let public_id = encode_gateway_continuation_id("all-cancelled")?;
        assert_eq!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Missing
        );
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn pending_identity_lock_never_serializes_unrelated_continuations() -> anyhow::Result<()>
    {
        let registry = registry(44).await?;
        let first = registry.pending_identity_lock("owner-a/request-a");
        let same = registry.pending_identity_lock("owner-a/request-a");
        let unrelated = registry.pending_identity_lock("owner-b/request-b");
        let first_guard = first.lock().await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), same.lock())
                .await
                .is_err(),
            "the same continuation identity must serialize bind/rollback"
        );
        let unrelated_guard =
            tokio::time::timeout(std::time::Duration::from_millis(100), unrelated.lock())
                .await
                .map_err(|_| anyhow::anyhow!("unrelated identity was globally serialized"))?;
        drop(unrelated_guard);
        drop(first_guard);
        let same_guard = tokio::time::timeout(std::time::Duration::from_millis(100), same.lock())
            .await
            .map_err(|_| anyhow::anyhow!("same-identity lock did not release"))?;
        drop(same_guard);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_provisional_mapping_remains_missing_after_registry_restart()
    -> anyhow::Result<()> {
        let registry = registry(41).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("restart-pending", 301, "provider-restart");
        runtime.finalize(&attempt).await?;
        runtime.rollback(&attempt).await?;
        let restarted = ContinuationRegistry::new(
            registry.inner.db.clone(),
            registry.inner.keys.clone(),
            30,
            10,
        )?;
        let public_id = encode_gateway_continuation_id("restart-pending")?;
        assert_eq!(
            restarted.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Missing,
            "tracked rollback must remove provisional durable state before restart"
        );
        Ok(())
    }

    #[tokio::test]
    async fn provisional_row_is_missing_even_without_process_local_marker() -> anyhow::Result<()> {
        let registry = registry(44).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("durable-provisional", 401, "provider-provisional");
        runtime.finalize(&attempt).await?;

        let fresh = ContinuationRegistry::new(
            registry.inner.db.clone(),
            registry.inner.keys.clone(),
            30,
            10,
        )?;
        assert_eq!(
            fresh
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("durable-provisional")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "durable resolve exposed a prepare that another process cannot mask"
        );
        runtime.rollback(&attempt).await?;
        Ok(())
    }

    #[tokio::test]
    async fn independent_registry_cannot_resolve_delivery_ready_before_acknowledgement()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, observer_registry) =
            independent_file_registries(53).await?;
        let runtime = ContinuationRuntime::new(owner_registry);
        let attempt = finalization_context("cross-registry-ready", 410, "provider-ready");
        runtime.finalize(&attempt).await?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new(ready_tx, ack_rx);
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;

        let public_id = encode_gateway_continuation_id("cross-registry-ready")?;
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "delivery readiness is not proof that downstream accepted the terminal"
        );

        ack_tx
            .send(DeliveryAcknowledgement::Failed(BitrouterError::internal(
                "terminal encoding failed",
            )))
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        commit
            .await?
            .expect_err("a negative acknowledgement must fail the commit");
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "negative acknowledgement must leave no resolvable publication"
        );
        Ok(())
    }

    #[tokio::test]
    async fn independent_registry_never_observes_early_delivery_drop_as_active()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, observer_registry) =
            independent_file_registries(63).await?;
        let runtime = ContinuationRuntime::new(owner_registry);
        let attempt = finalization_context("cross-registry-drop", 419, "provider-early-drop");
        runtime.finalize(&attempt).await?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new(ready_tx, ack_rx);
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        drop(ack_tx);

        let public_id = encode_gateway_continuation_id("cross-registry-drop")?;
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "dropping before acknowledgement must never expose Active"
        );
        assert!(!commit.await??);
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "early drop compensation must remove durable pre-delivery state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dropped_delivery_after_ack_compensates_owned_activation() -> anyhow::Result<()> {
        let (_directory, owner_registry, observer_registry) =
            independent_file_registries(54).await?;
        let runtime = ContinuationRuntime::new(owner_registry);
        let attempt = finalization_context("drop-after-ack", 411, "provider-drop-after-ack");
        runtime.finalize(&attempt).await?;
        let public_id = encode_gateway_continuation_id("drop-after-ack")?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        drop(activation_rx);
        drop(terminal_tx);

        assert!(!commit.await??);
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "dropping the delivery future after ack must compensate its owned generation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dropped_terminal_commit_after_active_cas_compensates_owned_activation()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, observer_registry) =
            independent_file_registries(55).await?;
        let runtime = ContinuationRuntime::new(owner_registry);
        let attempt = finalization_context("drop-after-active", 412, "provider-drop-after-active");
        runtime.finalize(&attempt).await?;
        let public_id = encode_gateway_continuation_id("drop-after-active")?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx.await??;
        assert!(matches!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Active(_)
        ));
        drop(terminal_tx);

        assert!(!commit.await??);
        assert_eq!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "an unobserved completion must not strand the active generation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn committed_terminal_is_immediately_active_to_independent_registry() -> anyhow::Result<()>
    {
        let (_directory, owner_registry, observer_registry) =
            independent_file_registries(56).await?;
        let runtime = ContinuationRuntime::new(owner_registry);
        let attempt = finalization_context("terminal-committed", 413, "provider-committed");
        runtime.finalize(&attempt).await?;
        let public_id = encode_gateway_continuation_id("terminal-committed")?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx.await??;
        terminal_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("terminal commit receiver closed"))?;
        assert!(commit.await??);

        assert!(matches!(
            observer_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn acknowledged_attempt_never_activates_a_rebound_foreign_generation()
    -> anyhow::Result<()> {
        let (_directory, first_registry, second_registry) = independent_file_registries(62).await?;
        let first_runtime = ContinuationRuntime::new(first_registry.clone());
        let second_runtime = ContinuationRuntime::new(second_registry.clone());
        let first = finalization_context("ack-rebind", 417, "provider-ack-rebind");
        let second = finalization_context("ack-rebind", 418, "provider-ack-rebind");
        first_runtime.finalize(&first).await?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = first_runtime.clone();
        let committing_attempt = first.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;

        let public_id = encode_gateway_continuation_id("ack-rebind")?;
        let continuation_identity = first_registry
            .inner
            .keys
            .load()?
            .continuation_identity("owner", &public_id)?;
        continuation_entity::Entity::delete_by_id(continuation_identity)
            .exec(second_registry.database())
            .await?;
        second_runtime.finalize(&second).await?;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx
            .await?
            .expect_err("foreign generation must reject activation");
        drop(terminal_tx);
        commit
            .await?
            .expect_err("the stale acknowledged attempt must fail closed");
        assert_eq!(
            second_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "stale acknowledgement must not activate the rebound provisional row"
        );

        commit_delivered(&second_runtime, &second).await?;
        assert!(matches!(
            second_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn failed_compensation_keeps_retryable_row_ownership_and_visibility_mask()
    -> anyhow::Result<()> {
        let registry = registry(45).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("rollback-retry", 402, "provider-rollback-retry");
        runtime.finalize(&attempt).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TRIGGER fail_continuation_delete BEFORE DELETE ON provider_continuations BEGIN SELECT RAISE(FAIL, 'forced rollback failure'); END".to_owned(),
            ))
            .await?;

        runtime
            .rollback(&attempt)
            .await
            .expect_err("forced conditional delete failure must propagate");
        assert_eq!(
            registry
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("rollback-retry")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "a failed compensation must remain masked in the same process"
        );
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER fail_continuation_delete".to_owned(),
            ))
            .await?;
        runtime.rollback(&attempt).await?;

        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(
            row.try_get::<i64>("", "count")?,
            0,
            "retry lost ownership before the conditional delete committed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn active_compensation_failure_remains_owned_for_outer_rollback_retry()
    -> anyhow::Result<()> {
        let registry = registry(46).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("active-rollback-retry", 403, "provider-active-retry");
        runtime.finalize(&attempt).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TRIGGER fail_active_continuation_demote BEFORE UPDATE OF publication_state ON provider_continuations WHEN OLD.publication_state = 'active' AND NEW.publication_state = 'provisional' BEGIN SELECT RAISE(FAIL, 'forced active demotion failure'); END".to_owned(),
            ))
            .await?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx.await??;
        drop(terminal_tx);
        commit
            .await?
            .expect_err("forced active compensation failure must propagate");
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER fail_active_continuation_demote".to_owned(),
            ))
            .await?;
        runtime.rollback(&attempt).await?;

        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(
            row.try_get::<i64>("", "count")?,
            0,
            "outer rollback lost ownership after active compensation failed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn demoted_delete_failure_remains_owned_for_outer_rollback_retry() -> anyhow::Result<()> {
        let registry = registry(51).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("demoted-delete-retry", 408, "provider-delete-retry");
        runtime.finalize(&attempt).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TRIGGER fail_owned_continuation_delete BEFORE DELETE ON provider_continuations WHEN OLD.publication_state = 'provisional' BEGIN SELECT RAISE(FAIL, 'forced owned delete failure'); END".to_owned(),
            ))
            .await?;

        commit_disconnected(&runtime, &attempt)
            .await
            .expect_err("forced provisional delete failure must propagate");
        assert_eq!(
            registry
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("demoted-delete-retry")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "a demoted but undeleted row must remain unpublished"
        );
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER fail_owned_continuation_delete".to_owned(),
            ))
            .await?;
        runtime.rollback(&attempt).await?;
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn direct_bind_cannot_promote_another_attempts_provisional_row() -> anyhow::Result<()> {
        let registry = registry(47).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("foreign-provisional", 404, "provider-provisional");
        runtime.finalize(&attempt).await?;

        let independent = ContinuationRegistry::new(
            registry.inner.db.clone(),
            registry.inner.keys.clone(),
            30,
            10,
        )?;
        independent
            .bind(
                "owner",
                &encode_gateway_continuation_id("foreign-provisional")?,
                "provider-provisional",
                &target("credential"),
                &static_authority("credential"),
                Utc::now(),
            )
            .await
            .expect_err("a public bind must not take ownership of a provisional publication");
        assert_eq!(
            independent
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("foreign-provisional")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "the foreign bind must not publish another attempt's row"
        );
        runtime.rollback(&attempt).await?;
        Ok(())
    }

    #[tokio::test]
    async fn independent_registries_never_share_provisional_attempt_ownership() -> anyhow::Result<()>
    {
        let first_registry = registry(48).await?;
        let second_registry = ContinuationRegistry::new(
            first_registry.inner.db.clone(),
            first_registry.inner.keys.clone(),
            30,
            10,
        )?;
        let first_runtime = ContinuationRuntime::new(first_registry.clone());
        let second_runtime = ContinuationRuntime::new(second_registry);
        let first_attempt = finalization_context("independent-owner", 405, "provider-independent");
        let second_attempt = finalization_context("independent-owner", 406, "provider-independent");
        first_runtime.finalize(&first_attempt).await?;

        second_runtime
            .finalize(&second_attempt)
            .await
            .expect_err("a second registry must not adopt the first registry's provisional row");
        second_runtime.rollback(&second_attempt).await?;
        assert_eq!(
            first_registry
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("independent-owner")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "the foreign rollback must leave the first attempt unpublished and intact"
        );
        let row = first_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 1);

        first_runtime.rollback(&first_attempt).await?;
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_delivery_attempt_id_never_overwrites_existing_ownership()
    -> anyhow::Result<()> {
        let registry = registry(52).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let first = finalization_context("attempt-owner-a", 409, "provider-attempt-a");
        let second = finalization_context("attempt-owner-b", 409, "provider-attempt-b");
        runtime.finalize(&first).await?;
        runtime
            .finalize(&second)
            .await
            .expect_err("duplicate process-local attempt ids must fail closed");
        runtime.rollback(&first).await?;

        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(
            row.try_get::<i64>("", "count")?,
            0,
            "the duplicate attempt must not replace the first row's rollback token"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pipeline_duplicate_finalize_rollback_is_invocation_attributed() -> anyhow::Result<()> {
        for (
            index,
            first_request_id,
            duplicate_request_id,
            first_provider_id,
            duplicate_provider_id,
        ) in [
            (
                0_u8,
                "pipeline-attempt-owner-a",
                "pipeline-attempt-owner-b",
                "provider-attempt-a",
                "provider-attempt-b",
            ),
            (
                1_u8,
                "pipeline-same-identity",
                "pipeline-same-identity",
                "provider-binding-a",
                "provider-binding-b",
            ),
            (
                2_u8,
                "pipeline-exact-duplicate",
                "pipeline-exact-duplicate",
                "provider-exact",
                "provider-exact",
            ),
        ] {
            let registry = registry(70 + index).await?;
            let runtime = ContinuationRuntime::new(registry.clone());
            let target = target("credential");
            let routes = Arc::new(StaticRoutingTable::new());
            routes.insert("gpt-5", vec![target.clone()]);
            let first_commit_started = Arc::new(tokio::sync::Notify::new());
            let release_first_commit = Arc::new(tokio::sync::Notify::new());
            let finalizer = CollidingAttemptFinalizer {
                runtime,
                forced_delivery_attempt_id: 430,
                blocked_request_id: first_request_id.to_owned(),
                first_commit_started: first_commit_started.clone(),
                release_first_commit: release_first_commit.clone(),
                duplicate_rollback: None,
            };
            let executor = Arc::new(MockExecutor::new(vec![
                MockResponse::Generate(valid_nonstream_result(&target, first_provider_id).result),
                MockResponse::Generate(
                    valid_nonstream_result(&target, duplicate_provider_id).result,
                ),
            ]));
            let mut builder = PipelineBuilder::new();
            builder
                .routing_table(routes)
                .executor(executor)
                .route_hook(finalizer.runtime.clone())
                .required_finalizer(finalizer);
            let pipeline = Arc::new(builder.build()?);

            let first = tokio::spawn({
                let pipeline = pipeline.clone();
                let request = nonstream_tool_request(first_request_id, None);
                async move { pipeline.execute(request).await }
            });
            first_commit_started.notified().await;

            let duplicate_error = pipeline
                .execute(nonstream_tool_request(duplicate_request_id, None))
                .await
                .err()
                .ok_or_else(|| {
                    anyhow::anyhow!("duplicate pipeline finalize unexpectedly succeeded")
                })?;
            assert!(
                duplicate_error
                    .to_string()
                    .contains("duplicate continuation delivery attempt id"),
                "unexpected duplicate error: {duplicate_error}"
            );
            assert_eq!(
                pending_publication_count(&registry),
                1,
                "automatic duplicate-context rollback removed the first invocation marker"
            );
            let public_id = encode_gateway_continuation_id(first_request_id)?;
            let continuation_identity = registry
                .inner
                .keys
                .load()?
                .continuation_identity("tool-owner", &public_id)?;
            let first_row = continuation_entity::Entity::find_by_id(continuation_identity)
                .one(registry.database())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "automatic duplicate-context rollback removed the first durable row"
                    )
                })?;
            assert_eq!(first_row.publication_state, PUBLICATION_PROVISIONAL);

            if index == 0 {
                first.abort();
                let _ = first.await;
                release_first_commit.notify_one();
                pipeline.drain_pending_settlements().await;
                assert_eq!(
                    pending_publication_count(&registry),
                    0,
                    "the original invocation receipt did not clear its owner marker"
                );
                assert!(
                    continuation_entity::Entity::find_by_id(&first_row.continuation_identity)
                        .one(registry.database())
                        .await?
                        .is_none(),
                    "the original invocation receipt did not delete its durable provisional row"
                );
                assert_eq!(
                    registry
                        .resolve("tool-owner", &public_id, Utc::now())
                        .await?,
                    ContinuationResolution::Missing,
                    "the original invocation receipt could not roll back its own publication"
                );
            } else {
                release_first_commit.notify_one();
                first.await??;
                pipeline.drain_pending_settlements().await;
                assert!(matches!(
                    registry
                        .resolve("tool-owner", &public_id, Utc::now())
                        .await?,
                    ContinuationResolution::Active(_)
                ));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn pipeline_late_duplicate_rollback_preserves_committed_first_invocation()
    -> anyhow::Result<()> {
        let registry = registry(73).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let target = target("credential");
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target.clone()]);
        let first_commit_started = Arc::new(tokio::sync::Notify::new());
        let release_first_commit = Arc::new(tokio::sync::Notify::new());
        let duplicate_rollback_started = Arc::new(tokio::sync::Notify::new());
        let release_duplicate_rollback = Arc::new(tokio::sync::Notify::new());
        let finalizer = CollidingAttemptFinalizer {
            runtime,
            forced_delivery_attempt_id: 431,
            blocked_request_id: "pipeline-late-owner-a".to_owned(),
            first_commit_started: first_commit_started.clone(),
            release_first_commit: release_first_commit.clone(),
            duplicate_rollback: Some(DuplicateRollbackBarrier {
                request_id: "pipeline-late-owner-b".to_owned(),
                started: duplicate_rollback_started.clone(),
                release: release_duplicate_rollback.clone(),
            }),
        };
        let executor = Arc::new(MockExecutor::new(vec![
            MockResponse::Generate(valid_nonstream_result(&target, "provider-late-owner-a").result),
            MockResponse::Generate(valid_nonstream_result(&target, "provider-late-owner-b").result),
        ]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(finalizer.runtime.clone())
            .required_finalizer(finalizer);
        let pipeline = Arc::new(builder.build()?);

        let first = tokio::spawn({
            let pipeline = pipeline.clone();
            async move {
                pipeline
                    .execute(nonstream_tool_request("pipeline-late-owner-a", None))
                    .await
            }
        });
        first_commit_started.notified().await;
        let duplicate = tokio::spawn({
            let pipeline = pipeline.clone();
            async move {
                pipeline
                    .execute(nonstream_tool_request("pipeline-late-owner-b", None))
                    .await
            }
        });
        duplicate_rollback_started.notified().await;

        release_first_commit.notify_one();
        first.await??;
        release_duplicate_rollback.notify_one();
        assert!(duplicate.await?.is_err());
        pipeline.drain_pending_settlements().await;
        assert!(matches!(
            registry
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id("pipeline-late-owner-a")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn reused_successful_http_root_never_accumulates_pending_publications()
    -> anyhow::Result<()> {
        let registry = registry(64).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let upstream_target = target("credential");
        let responses = (0..4)
            .map(|index| {
                MockResponse::Generate(
                    valid_nonstream_result(
                        &upstream_target,
                        &format!("provider-reused-root-{index}"),
                    )
                    .result,
                )
            })
            .collect();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(MockExecutor::new(responses)))
            .route_hook(runtime.clone())
            .required_finalizer(runtime);
        let pipeline = Arc::new(builder.build()?);
        let app = build_router(app_state(pipeline.clone()));
        let request_id = "reused-successful-http-root";

        let first = app
            .clone()
            .oneshot(responses_http_request(request_id, false))
            .await?;
        assert_eq!(first.status(), axum::http::StatusCode::OK);
        pipeline.drain_pending_settlements().await;
        assert_eq!(pending_publication_count(&registry), 0);

        let public_id = encode_gateway_continuation_id(request_id)?;
        let continuation_identity = registry
            .inner
            .keys
            .load()?
            .continuation_identity(CallerContext::local().user_id(), &public_id)?;
        let original_row = continuation_entity::Entity::find_by_id(&continuation_identity)
            .one(registry.database())
            .await?
            .ok_or_else(|| anyhow::anyhow!("initial active continuation missing"))?;
        assert_eq!(original_row.publication_state, PUBLICATION_ACTIVE);

        for collision_index in 1..4 {
            let collision = app
                .clone()
                .oneshot(responses_http_request(request_id, false))
                .await?;
            assert_eq!(
                collision.status(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "reused root collision {collision_index} did not fail closed"
            );
            pipeline.drain_pending_settlements().await;
            assert_eq!(
                pending_publication_count(&registry),
                0,
                "reused root collision {collision_index} leaked local publication ownership"
            );
            let current_row = continuation_entity::Entity::find_by_id(&continuation_identity)
                .one(registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("original active continuation disappeared"))?;
            assert_eq!(current_row, original_row);
            let ContinuationResolution::Active(active) = registry
                .resolve(CallerContext::local().user_id(), &public_id, Utc::now())
                .await?
            else {
                anyhow::bail!("original continuation stopped resolving as active")
            };
            assert_eq!(active.provider_response_id, "provider-reused-root-0");
        }
        Ok(())
    }

    #[tokio::test]
    async fn foreign_provisional_generation_with_different_binding_clears_only_contender()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, contender_registry) =
            independent_file_registries(65).await?;
        let owner_runtime = ContinuationRuntime::new(owner_registry.clone());
        let contender_runtime = ContinuationRuntime::new(contender_registry.clone());
        let owner = finalization_context("foreign-provisional-owner", 417, "provider-owner");
        let contender =
            finalization_context("foreign-provisional-owner", 418, "provider-contender");

        owner_runtime.finalize(&owner).await?;
        let collision_error = match contender_runtime.finalize(&contender).await {
            Ok(()) => anyhow::bail!("a different provisional binding unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            collision_error
                .to_string()
                .contains("already bound to another response"),
            "tracked rollback replaced the original provisional collision: {collision_error}"
        );
        assert!(contender_runtime.rollback(&contender).await.is_ok());
        assert_eq!(
            pending_publication_count(&contender_registry),
            0,
            "foreign provisional ownership leaked the contender marker"
        );
        assert_eq!(pending_publication_count(&owner_registry), 1);
        owner_runtime.rollback(&owner).await?;
        Ok(())
    }

    #[tokio::test]
    async fn foreign_delivering_generation_with_different_binding_clears_only_contender()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, contender_registry) =
            independent_file_registries(66).await?;
        let owner_runtime = ContinuationRuntime::new(owner_registry.clone());
        let contender_runtime = ContinuationRuntime::new(contender_registry.clone());
        let owner = finalization_context("foreign-delivering-owner", 419, "provider-owner");
        let contender = finalization_context("foreign-delivering-owner", 420, "provider-contender");
        owner_runtime.finalize(&owner).await?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new(ready_tx, ack_rx);
        let committing_runtime = owner_runtime.clone();
        let committing_owner = owner.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_owner, &delivery)
                .await
        });
        ready_rx.await??;

        let collision_error = match contender_runtime.finalize(&contender).await {
            Ok(()) => anyhow::bail!("a different delivering binding unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            collision_error
                .to_string()
                .contains("already bound to another response"),
            "tracked rollback replaced the original delivering collision: {collision_error}"
        );
        assert!(contender_runtime.rollback(&contender).await.is_ok());
        assert_eq!(
            pending_publication_count(&contender_registry),
            0,
            "foreign delivering ownership leaked the contender marker"
        );

        drop(ack_tx);
        assert!(!commit.await??);
        assert_eq!(pending_publication_count(&owner_registry), 0);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_insert_with_absent_reread_clears_local_marker() -> anyhow::Result<()> {
        let registry = registry(67).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TRIGGER reject_continuation_insert BEFORE INSERT ON provider_continuations BEGIN SELECT RAISE(ABORT, 'injected insert rejection'); END".to_owned(),
            ))
            .await?;
        let attempt = finalization_context("absent-after-insert-error", 421, "provider-absent");

        let insert_error = match runtime.finalize(&attempt).await {
            Ok(()) => anyhow::bail!("the rejected insert unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            insert_error
                .to_string()
                .contains("injected insert rejection"),
            "tracked rollback replaced the original insert failure: {insert_error}"
        );
        assert_eq!(
            pending_publication_count(&registry),
            0,
            "a reliable absent reread retained impossible publication ownership"
        );
        runtime.rollback(&attempt).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER reject_continuation_insert".to_owned(),
            ))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn database_uncertainty_retains_marker_until_rollback_can_decide() -> anyhow::Result<()> {
        let (_directory, registry, database_control) = independent_file_registries(68).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("unknown-owned-generation", 422, "provider-unknown");
        let (committed, release) = registry.inject_ambiguous_insert_after_commit();
        let finalizing_runtime = runtime.clone();
        let finalizing_attempt = attempt.clone();
        let finalize =
            tokio::spawn(async move { finalizing_runtime.finalize(&finalizing_attempt).await });

        committed.await?;
        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                    .to_owned(),
            ))
            .await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("ambiguous insert release closed"))?;
        assert!(finalize.await?.is_err());
        assert_eq!(
            pending_publication_count(&registry),
            1,
            "an unknown reread discarded possible committed ownership"
        );

        assert!(runtime.rollback(&attempt).await.is_err());
        assert_eq!(
            pending_publication_count(&registry),
            1,
            "an unknown compensation error discarded retry ownership"
        );
        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                    .to_owned(),
            ))
            .await?;
        runtime.rollback(&attempt).await?;
        assert_eq!(pending_publication_count(&registry), 0);
        assert_eq!(
            database_control
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("unknown-owned-generation")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn assembled_http_unknown_publications_reconcile_after_database_recovery()
    -> anyhow::Result<()> {
        let (_directory, registry, database_control) = independent_file_registries(74).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let target = target("credential");
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target.clone()]);
        let responses = (0..3)
            .map(|index| {
                MockResponse::Generate(
                    valid_nonstream_result(&target, &format!("provider-unknown-http-{index}"))
                        .result,
                )
            })
            .collect();
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(MockExecutor::new(responses)))
            .route_hook(runtime.clone())
            .required_finalizer(runtime);
        let pipeline = Arc::new(builder.build()?);
        let app = build_router(app_state(pipeline.clone()));
        let mut retained_counts = Vec::new();

        for index in 0..3 {
            let (committed, release) = registry.inject_ambiguous_insert_after_commit();
            let response = tokio::spawn({
                let app = app.clone();
                async move {
                    app.oneshot(responses_http_request(
                        &format!("unknown-http-{index}"),
                        false,
                    ))
                    .await
                }
            });
            committed.await?;
            database_control
                .database()
                .execute(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                        .to_owned(),
                ))
                .await?;
            release
                .send(())
                .map_err(|_| anyhow::anyhow!("ambiguous insert release closed"))?;
            let response = response.await??;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            );
            database_control
                .database()
                .execute(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                        .to_owned(),
                ))
                .await?;
            pipeline.drain_pending_settlements().await;
            retained_counts.push(pending_publication_count(&registry));
        }

        pipeline.drain_pending_settlements().await;
        assert_eq!(
            retained_counts,
            vec![0, 0, 0],
            "automatic production reconciliation retained Unknown markers after recovery"
        );
        assert_eq!(pending_publication_count(&registry), 0);
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(
            row.try_get::<i64>("", "count")?,
            0,
            "automatic production reconciliation retained durable provisional rows"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reconciliation_capacity_backpressures_before_durable_side_effect() -> anyhow::Result<()>
    {
        let mut registry = registry(75).await?;
        let inner = Arc::get_mut(&mut registry.inner)
            .ok_or_else(|| anyhow::anyhow!("registry unexpectedly shared before configuration"))?;
        inner.pending_capacity = 1;
        let runtime = ContinuationRuntime::new(registry.clone());
        let first = finalization_context("capacity-owner-a", 440, "provider-capacity-a");
        let second = finalization_context("capacity-owner-b", 441, "provider-capacity-b");

        runtime.finalize(&first).await?;
        let worker = reconciler_task_id(&registry)
            .ok_or_else(|| anyhow::anyhow!("reconciliation worker did not start"))?;
        let error = runtime
            .finalize(&second)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("capacity exhaustion unexpectedly succeeded"))?;
        assert!(error.to_string().contains("capacity exhausted"));
        assert_eq!(pending_publication_count(&registry), 1);
        assert_eq!(reconciler_task_id(&registry), Some(worker));
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 1);
        runtime.rollback(&first).await?;
        Ok(())
    }

    #[tokio::test]
    async fn reserved_bind_is_never_scanned_or_compensated_by_worker() -> anyhow::Result<()> {
        let registry = registry(76).await?;
        registry.start_reconciler();
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("reserved-bind", 442, "provider-reserved");
        let (committed, release) = registry.inject_ambiguous_insert_after_commit();
        let finalizing = tokio::spawn({
            let runtime = runtime.clone();
            let attempt = attempt.clone();
            async move { runtime.finalize(&attempt).await }
        });
        committed.await?;
        let phase = registry
            .pending_publication(attempt.delivery_attempt_id, None)
            .map(|publication| publication.phase)
            .ok_or_else(|| anyhow::anyhow!("reserved publication marker missing"))?;
        assert!(matches!(phase, PendingPublicationPhase::Reserved));

        registry
            .reconcile_pass(Utc::now() + TimeDelta::seconds(60))
            .await;
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(
            row.try_get::<i64>("", "count")?,
            1,
            "worker compensated a bind still in Reserved phase"
        );

        release
            .send(())
            .map_err(|_| anyhow::anyhow!("ambiguous insert release closed"))?;
        finalizing.await??;
        runtime.rollback(&attempt).await?;
        Ok(())
    }

    #[tokio::test]
    async fn persistent_outage_keeps_one_worker_and_bounded_owner_evidence() -> anyhow::Result<()> {
        let (_directory, mut registry, database_control) = independent_file_registries(77).await?;
        let inner = Arc::get_mut(&mut registry.inner)
            .ok_or_else(|| anyhow::anyhow!("registry unexpectedly shared before configuration"))?;
        inner.pending_capacity = 3;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempts = (0_u64..3)
            .map(|index| {
                finalization_context(
                    &format!("persistent-outage-{index}"),
                    443 + index,
                    &format!("provider-persistent-{index}"),
                )
            })
            .collect::<Vec<_>>();
        for attempt in &attempts {
            runtime.finalize(attempt).await?;
        }
        let worker = reconciler_task_id(&registry)
            .ok_or_else(|| anyhow::anyhow!("reconciliation worker did not start"))?;
        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                    .to_owned(),
            ))
            .await?;
        for attempt in &attempts {
            assert!(runtime.rollback(attempt).await.is_err());
        }
        assert_eq!(pending_publication_count(&registry), 3);
        assert_eq!(reconciler_task_id(&registry), Some(worker));
        let overflow = finalization_context("persistent-overflow", 446, "provider-overflow");
        let error =
            runtime.finalize(&overflow).await.err().ok_or_else(|| {
                anyhow::anyhow!("persistent-outage overflow unexpectedly succeeded")
            })?;
        assert!(error.to_string().contains("capacity exhausted"));
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(pending_publication_count(&registry), 3);
        assert_eq!(reconciler_task_id(&registry), Some(worker));

        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                    .to_owned(),
            ))
            .await?;
        wait_for_pending_publication_count(&registry, 0).await?;
        assert_eq!(reconciler_task_id(&registry), Some(worker));
        Ok(())
    }

    #[tokio::test]
    async fn reconciler_round_robin_renews_more_than_one_batch_without_self_claim()
    -> anyhow::Result<()> {
        let registry = registry(78).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut attempts = (0_u64..48)
            .map(|index| {
                finalization_context(
                    &format!("round-robin-{index}"),
                    500 + index,
                    &format!("provider-round-robin-{index}"),
                )
            })
            .collect::<Vec<_>>();
        for attempt in &attempts {
            runtime.finalize(attempt).await?;
        }
        suspend_reconciler_for_test(&registry).await;
        match registry.inner.reconciler.pending_cursor.lock() {
            Ok(mut cursor) => *cursor = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
        let future = Utc::now() + TimeDelta::seconds(20);
        registry.reconcile_pass(future).await;

        for removed in attempts.drain(0..4) {
            runtime.rollback(&removed).await?;
        }
        let replacements = (0_u64..4)
            .map(|index| {
                finalization_context(
                    &format!("round-robin-replacement-{index}"),
                    100 + index,
                    &format!("provider-round-robin-replacement-{index}"),
                )
            })
            .collect::<Vec<_>>();
        for replacement in &replacements {
            runtime.finalize(replacement).await?;
        }
        attempts.extend(replacements);
        suspend_reconciler_for_test(&registry).await;
        match registry.inner.reconciler.pending_cursor.lock() {
            Ok(mut cursor) => *cursor = Some(531),
            Err(poisoned) => *poisoned.into_inner() = Some(531),
        }
        registry.reconcile_pass(future).await;
        let renewed = registry
            .database()
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations WHERE publication_lease_until > ?",
                [timestamp(future + TimeDelta::seconds(15)).into()],
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("renewed continuation count missing"))?;
        assert_eq!(
            renewed.try_get::<i64>("", "count")?,
            48,
            "stable round-robin skipped owner evidence after removal and low-id insertion"
        );
        registry
            .reconcile_pass(future + TimeDelta::seconds(15))
            .await;
        assert_eq!(pending_publication_count(&registry), attempts.len());
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 48);
        for attempt in &attempts {
            runtime.rollback(attempt).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn graceful_shutdown_drains_more_than_one_recovered_batch() -> anyhow::Result<()> {
        let (_directory, registry, database_control) = independent_file_registries(79).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempts = (0_u64..48)
            .map(|index| {
                finalization_context(
                    &format!("shutdown-batch-{index}"),
                    600 + index,
                    &format!("provider-shutdown-batch-{index}"),
                )
            })
            .collect::<Vec<_>>();
        for attempt in &attempts {
            runtime.finalize(attempt).await?;
        }
        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                    .to_owned(),
            ))
            .await?;
        for attempt in &attempts {
            assert!(runtime.rollback(attempt).await.is_err());
        }
        assert_eq!(pending_publication_count(&registry), attempts.len());
        database_control
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                    .to_owned(),
            ))
            .await?;

        tokio::time::timeout(Duration::from_secs(70), registry.stop_reconciler()).await??;
        assert_eq!(pending_publication_count(&registry), 0);
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn restart_claims_only_expired_prepublication_leases() -> anyhow::Result<()> {
        let (_directory, owner_registry, restarted_registry) =
            independent_file_registries(80).await?;
        assert_ne!(
            owner_registry.inner.instance_id,
            restarted_registry.inner.instance_id
        );
        let runtime = ContinuationRuntime::new(owner_registry.clone());
        let provisional = finalization_context("restart-provisional", 700, "provider-provisional");
        let delivering = finalization_context("restart-delivering", 701, "provider-delivering");
        let raced = finalization_context("restart-raced", 705, "provider-raced");
        runtime.finalize(&provisional).await?;
        runtime.finalize(&delivering).await?;
        runtime.finalize(&raced).await?;
        suspend_reconciler_for_test(&owner_registry).await;

        let delivering_publication = owner_registry
            .pending_publication(delivering.delivery_attempt_id, None)
            .ok_or_else(|| anyhow::anyhow!("delivering owner marker missing"))?;
        let delivering_row =
            continuation_entity::Entity::find_by_id(&delivering_publication.continuation_identity)
                .one(owner_registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("delivering durable row missing"))?;
        owner_registry
            .transition_publication_state(
                &owner_registry.inner.keys.load()?,
                &delivering_row,
                PUBLICATION_DELIVERING,
            )
            .await?;
        owner_registry
            .bind(
                "owner",
                "restart-active",
                "provider-active",
                &target("credential"),
                &static_authority("credential"),
                Utc::now(),
            )
            .await?;

        let raced_publication = owner_registry
            .pending_publication(raced.delivery_attempt_id, None)
            .ok_or_else(|| anyhow::anyhow!("raced owner marker missing"))?;
        let stale_raced_row =
            continuation_entity::Entity::find_by_id(&raced_publication.continuation_identity)
                .one(owner_registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("raced durable row missing"))?;
        let renewal_time = Utc::now() + TimeDelta::seconds(20);
        for attempt in [&provisional, &delivering, &raced] {
            let publication = owner_registry
                .pending_publication(attempt.delivery_attempt_id, None)
                .ok_or_else(|| anyhow::anyhow!("owner marker missing before renewal"))?;
            owner_registry
                .renew_publication_lease(attempt.delivery_attempt_id, &publication, renewal_time)
                .await
                .map_err(OwnershipFailure::into_error)?;
        }
        let renewed_raced_row =
            continuation_entity::Entity::find_by_id(&raced_publication.continuation_identity)
                .one(owner_registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("renewed raced row missing"))?;
        assert_eq!(
            decrypt_payload(&owner_registry.inner.keys.load()?, &renewed_raced_row)?
                .provider_response_id,
            "provider-raced",
            "lease renewal did not re-seal an authenticated readable row"
        );
        restarted_registry
            .claim_and_compensate_stale_publication(
                stale_raced_row,
                renewal_time + TimeDelta::seconds(15),
            )
            .await?;
        let post_claim_raced_row =
            continuation_entity::Entity::find_by_id(&raced_publication.continuation_identity)
                .one(owner_registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("renewed row disappeared after stale claim"))?;
        assert_eq!(
            post_claim_raced_row, renewed_raced_row,
            "stale claim snapshot replaced a concurrently renewed owner lease"
        );
        runtime.rollback(&raced).await?;

        restarted_registry
            .reconcile_pass(renewal_time + TimeDelta::seconds(15))
            .await;
        let live_rows = restarted_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("live continuation count missing"))?;
        assert_eq!(
            live_rows.try_get::<i64>("", "count")?,
            3,
            "restart claimed a lease still renewed by its originating instance"
        );

        drop(runtime);
        drop(owner_registry);
        restarted_registry
            .reconcile_pass(renewal_time + TimeDelta::seconds(31))
            .await;
        let expired_rows = restarted_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("expired continuation count missing"))?;
        assert_eq!(expired_rows.try_get::<i64>("", "count")?, 1);
        assert!(matches!(
            restarted_registry
                .resolve(
                    "owner",
                    "restart-active",
                    renewal_time + TimeDelta::seconds(31)
                )
                .await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn active_unknown_is_masked_and_reconciles_but_restart_leaves_it_active()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, restarted_registry) =
            independent_file_registries(81).await?;
        let runtime = ContinuationRuntime::new(owner_registry.clone());
        let attempt = finalization_context("active-unknown-auto", 702, "provider-active-unknown");
        runtime.finalize(&attempt).await?;
        suspend_reconciler_for_test(&owner_registry).await;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx.await??;
        restarted_registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                    .to_owned(),
            ))
            .await?;
        drop(terminal_tx);
        assert!(commit.await?.is_err());
        suspend_reconciler_for_test(&owner_registry).await;
        restarted_registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                    .to_owned(),
            ))
            .await?;

        let public_id = encode_gateway_continuation_id("active-unknown-auto")?;
        assert_eq!(pending_publication_count(&owner_registry), 1);
        assert_eq!(
            owner_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "same-instance Active ambiguity was not masked"
        );
        assert!(matches!(
            restarted_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Active(_)
        ));
        restarted_registry
            .reconcile_pass(Utc::now() + TimeDelta::seconds(60))
            .await;
        assert!(matches!(
            restarted_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Active(_)
        ));

        owner_registry.start_reconciler();
        wait_for_pending_publication_count(&owner_registry, 0).await?;
        assert_eq!(
            restarted_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn required_shutdown_drain_retries_active_unknown_after_database_recovery()
    -> anyhow::Result<()> {
        let (_directory, owner_registry, restarted_registry) =
            independent_file_registries(83).await?;
        let runtime = ContinuationRuntime::new(owner_registry.clone());
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(MockExecutor::always_text("unused")))
            .required_finalizer(runtime.clone());
        let pipeline = Arc::new(builder.build()?);
        let attempt = finalization_context(
            "required-drain-active-unknown",
            706,
            "provider-required-drain-active-unknown",
        );
        runtime.finalize(&attempt).await?;
        suspend_reconciler_for_test(&owner_registry).await;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (activation_tx, activation_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let delivery = RequiredDeliveryHandshake::new_with_completion(
            ready_tx,
            ack_rx,
            activation_tx,
            terminal_rx,
        );
        let committing_runtime = runtime.clone();
        let committing_attempt = attempt.clone();
        let commit = tokio::spawn(async move {
            committing_runtime
                .commit(&committing_attempt, &delivery)
                .await
        });
        ready_rx.await??;
        ack_tx
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("delivery acknowledgement closed"))?;
        activation_rx.await??;
        let active = owner_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT publication_state FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("active continuation row missing"))?;
        assert_eq!(active.try_get::<String>("", "publication_state")?, "active");

        restarted_registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden"
                    .to_owned(),
            ))
            .await?;
        drop(terminal_tx);
        assert!(commit.await?.is_err());
        suspend_reconciler_for_test(&owner_registry).await;

        assert!(
            pipeline.drain_required_pending_settlements().await.is_err(),
            "required shutdown drain must propagate unresolved owner reconciliation"
        );
        assert_eq!(pending_publication_count(&owner_registry), 1);

        restarted_registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations"
                    .to_owned(),
            ))
            .await?;
        assert_eq!(
            pipeline.drain_required_pending_settlements().await?,
            0,
            "the retry should not invent detached pipeline work"
        );
        assert_eq!(pending_publication_count(&owner_registry), 0);
        let rows = restarted_registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(rows.try_get::<i64>("", "count")?, 0);

        let public_id = encode_gateway_continuation_id("required-drain-active-unknown")?;
        drop(pipeline);
        drop(runtime);
        drop(owner_registry);
        assert_eq!(
            restarted_registry
                .resolve("owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn worker_drops_lost_evidence_without_touching_foreign_generation() -> anyhow::Result<()>
    {
        let (_directory, owner_registry, contender_registry) =
            independent_file_registries(82).await?;
        let owner_runtime = ContinuationRuntime::new(owner_registry.clone());
        let contender_runtime = ContinuationRuntime::new(contender_registry.clone());
        let owner = finalization_context("worker-lost-owner", 703, "provider-owner");
        let contender = finalization_context("worker-lost-owner", 704, "provider-contender");
        owner_runtime.finalize(&owner).await?;
        suspend_reconciler_for_test(&owner_registry).await;
        let owner_publication = owner_registry
            .pending_publication(owner.delivery_attempt_id, None)
            .ok_or_else(|| anyhow::anyhow!("owner publication marker missing"))?;
        continuation_entity::Entity::delete_by_id(&owner_publication.continuation_identity)
            .exec(contender_registry.database())
            .await?;
        contender_runtime.finalize(&contender).await?;
        suspend_reconciler_for_test(&contender_registry).await;
        assert!(owner_registry.update_pending_phase(
            owner.delivery_attempt_id,
            &owner_publication.generation,
            None,
            PendingPublicationPhase::Reconcile,
        ));
        owner_registry.start_reconciler();
        wait_for_pending_publication_count(&owner_registry, 0).await?;
        assert_eq!(pending_publication_count(&contender_registry), 1);
        let contender_publication = contender_registry
            .pending_publication(contender.delivery_attempt_id, None)
            .ok_or_else(|| anyhow::anyhow!("contender publication marker missing"))?;
        let row =
            continuation_entity::Entity::find_by_id(&contender_publication.continuation_identity)
                .one(contender_registry.database())
                .await?
                .ok_or_else(|| anyhow::anyhow!("foreign durable generation disappeared"))?;
        assert_eq!(row.publication_generation, contender_publication.generation);
        contender_runtime.rollback(&contender).await?;
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_committed_insert_same_generation_retains_rollback_ownership()
    -> anyhow::Result<()> {
        let (_directory, registry, restarted_registry) = independent_file_registries(57).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let attempt = finalization_context("ambiguous-owned", 414, "provider-ambiguous-owned");
        let (committed, release) = registry.inject_ambiguous_insert_after_commit();
        let finalizing_runtime = runtime.clone();
        let finalizing_attempt = attempt.clone();
        let finalize =
            tokio::spawn(async move { finalizing_runtime.finalize(&finalizing_attempt).await });

        committed.await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("ambiguous insert release closed"))?;
        finalize
            .await?
            .expect("rereading the same durable generation must retain ownership");
        assert!(
            registry
                .inner
                .pending_publications
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&attempt.delivery_attempt_id),
            "ambiguous success must preserve the rollback generation"
        );

        runtime.rollback(&attempt).await?;
        assert_eq!(
            restarted_registry
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("ambiguous-owned")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "tracked rollback must clean ambiguous committed state across restart"
        );
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_insert_never_adopts_rebound_foreign_generation() -> anyhow::Result<()> {
        let (_directory, first_registry, second_registry) = independent_file_registries(58).await?;
        let first_runtime = ContinuationRuntime::new(first_registry.clone());
        let second_runtime = ContinuationRuntime::new(second_registry.clone());
        let first = finalization_context("ambiguous-foreign", 415, "provider-ambiguous-foreign");
        let second = finalization_context("ambiguous-foreign", 416, "provider-ambiguous-foreign");
        let (committed, release) = first_registry.inject_ambiguous_insert_after_commit();
        let finalizing_runtime = first_runtime.clone();
        let finalizing_attempt = first.clone();
        let finalize =
            tokio::spawn(async move { finalizing_runtime.finalize(&finalizing_attempt).await });

        committed.await?;
        let continuation_identity = first_registry.inner.keys.load()?.continuation_identity(
            "owner",
            &encode_gateway_continuation_id("ambiguous-foreign")?,
        )?;
        continuation_entity::Entity::delete_by_id(continuation_identity)
            .exec(second_registry.database())
            .await?;
        second_runtime.finalize(&second).await?;
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("ambiguous insert release closed"))?;
        finalize
            .await?
            .expect_err("a different durable generation must remain foreign");

        first_runtime.rollback(&first).await?;
        assert_eq!(
            second_registry
                .resolve(
                    "owner",
                    &encode_gateway_continuation_id("ambiguous-foreign")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "foreign provisional ownership remains unpublished"
        );
        second_runtime.rollback(&second).await?;
        Ok(())
    }

    #[derive(Default)]
    struct ToolRoundState {
        calls: usize,
        forwarded_parents: Vec<Option<String>>,
    }

    struct ToolRoundResponder(Arc<std::sync::Mutex<ToolRoundState>>);

    impl Respond for ToolRoundResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let mut state = self.0.lock().expect("tool round state poisoned");
            state.forwarded_parents.push(
                body.get("previous_response_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
            let call = state.calls;
            state.calls += 1;
            drop(state);

            let events = if call == 0 {
                let item = serde_json::json!({
                    "id": "fc-intermediate",
                    "type": "function_call",
                    "status": "completed",
                    "call_id": "call-search",
                    "name": "search",
                    "arguments": "{}"
                });
                vec![
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": "provider-intermediate", "status": "in_progress"}
                    }),
                    serde_json::json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {
                            "id": "fc-intermediate",
                            "type": "function_call",
                            "call_id": "call-search",
                            "name": "search",
                            "arguments": ""
                        }
                    }),
                    serde_json::json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": "fc-intermediate",
                        "output_index": 0,
                        "delta": "{}"
                    }),
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": item.clone()
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "provider-intermediate",
                            "status": "completed",
                            "output": [item]
                        }
                    }),
                ]
            } else {
                let response_id = if call == 1 {
                    "provider-final"
                } else {
                    "provider-next"
                };
                vec![
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": response_id, "status": "in_progress"}
                    }),
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "done"
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"id": response_id, "status": "completed", "output": []}
                    }),
                ]
            };
            let sse = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().expect("event type")
                    )
                })
                .collect::<String>();
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse)
        }
    }

    struct NonstreamToolRoundResponder(Arc<std::sync::Mutex<ToolRoundState>>);

    impl Respond for NonstreamToolRoundResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let mut state = self.0.lock().expect("tool round state poisoned");
            state.forwarded_parents.push(
                body.get("previous_response_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
            let call = state.calls;
            state.calls += 1;
            drop(state);

            if call == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "provider-intermediate",
                    "status": "completed",
                    "output": [{
                        "id": "fc-intermediate",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": "call-search",
                        "name": "search",
                        "arguments": "{}"
                    }],
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "total_tokens": 12
                    }
                }))
            } else {
                let response_id = if call == 1 {
                    "provider-final"
                } else {
                    "provider-next"
                };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": response_id,
                    "status": "completed",
                    "output": [{
                        "id": format!("msg-{call}"),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "done",
                            "annotations": []
                        }]
                    }],
                    "usage": {
                        "input_tokens": 20,
                        "output_tokens": 3,
                        "total_tokens": 23
                    }
                }))
            }
        }
    }

    struct ContinuationResponder {
        state: Arc<std::sync::Mutex<ToolRoundState>>,
        stream: bool,
    }

    impl Respond for ContinuationResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let mut state = self.state.lock().expect("continuation state poisoned");
            state.forwarded_parents.push(
                body.get("previous_response_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            );
            let response_id = if state.calls == 0 {
                "fallback-provider-root"
            } else {
                "fallback-provider-resumed"
            };
            state.calls += 1;
            drop(state);

            if self.stream {
                let events = [
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": response_id, "status": "in_progress"}
                    }),
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "done"
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": response_id,
                            "status": "completed",
                            "output": []
                        }
                    }),
                ];
                let sse = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().expect("event type")
                        )
                    })
                    .collect::<String>();
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": response_id,
                    "status": "completed",
                    "output": []
                }))
            }
        }
    }

    #[derive(Default)]
    struct DynamicAuthState {
        calls: Vec<(Option<String>, Option<String>)>,
    }

    struct DynamicAuthResponder(Arc<std::sync::Mutex<DynamicAuthState>>);

    impl Respond for DynamicAuthResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let authorization = request
                .headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let previous = body
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let mut state = self.0.lock().expect("dynamic auth state poisoned");
            state.calls.push((authorization, previous));
            let response_id = if state.calls.len() == 1 {
                "dynamic-provider-root"
            } else {
                "dynamic-provider-resumed"
            };
            drop(state);

            let events = [
                serde_json::json!({
                    "type": "response.created",
                    "response": {"id": response_id, "status": "in_progress"}
                }),
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "done"
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": response_id, "status": "completed", "output": []}
                }),
            ];
            let sse = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().expect("event type")
                    )
                })
                .collect::<String>();
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse)
        }
    }

    struct DynamicAuthApplier {
        credential: String,
    }

    #[async_trait]
    impl AuthApplier for DynamicAuthApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            let value =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.credential))
                    .map_err(|error| BitrouterError::internal(format!("test bearer: {error}")))?;
            request
                .headers_mut()
                .insert(reqwest::header::AUTHORIZATION, value);
            Ok(request)
        }

        async fn apply_with_authority(
            &self,
            request: reqwest::Request,
            target: &RoutingTarget,
        ) -> PipelineResult<AppliedAuth> {
            Ok(AppliedAuth::proven(
                self.apply(request, target).await?,
                CredentialAuthority::derive("test/dynamic-principal", &self.credential),
            ))
        }

        async fn continuation_authority(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<CredentialAuthority>> {
            Ok(Some(CredentialAuthority::derive(
                "test/dynamic-principal",
                &self.credential,
            )))
        }
    }

    struct UnsupportedDynamicAuthApplier;

    #[async_trait]
    impl AuthApplier for UnsupportedDynamicAuthApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer unsupported"),
            );
            Ok(request)
        }
    }

    struct FailingAuthorityAuthApplier;

    #[async_trait]
    impl AuthApplier for FailingAuthorityAuthApplier {
        async fn apply(
            &self,
            request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            Ok(request)
        }

        async fn continuation_authority(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<CredentialAuthority>> {
            Err(BitrouterError::internal(
                "broken non-Responses credential store",
            ))
        }
    }

    struct SecretFailingAuthorityAuthApplier;

    #[async_trait]
    impl AuthApplier for SecretFailingAuthorityAuthApplier {
        async fn apply(
            &self,
            request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            Ok(request)
        }

        async fn continuation_authority_proof(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<ContinuationAuthority>> {
            Err(BitrouterError::internal(
                "opaque authority plugin exposed opaque-authority-private-sentinel",
            ))
        }
    }

    struct SecretFailingApplyAuthApplier;

    #[async_trait]
    impl AuthApplier for SecretFailingApplyAuthApplier {
        async fn apply(
            &self,
            _request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            Err(BitrouterError::internal(
                "opaque apply plugin exposed opaque-public-auth-private-sentinel",
            ))
        }
    }

    struct RacedDynamicAuthApplier;

    #[async_trait]
    impl AuthApplier for RacedDynamicAuthApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer principal-b"),
            );
            Ok(request)
        }

        async fn apply_with_authority(
            &self,
            request: reqwest::Request,
            target: &RoutingTarget,
        ) -> PipelineResult<AppliedAuth> {
            Ok(AppliedAuth::proven(
                self.apply(request, target).await?,
                CredentialAuthority::derive("test/dynamic-principal", "principal-b"),
            ))
        }

        async fn continuation_authority(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<CredentialAuthority>> {
            Ok(Some(CredentialAuthority::derive(
                "test/dynamic-principal",
                "principal-a",
            )))
        }
    }

    struct SchemeAuthApplier(bitrouter_sdk::language_model::types::AuthScheme);

    #[async_trait]
    impl AuthApplier for SchemeAuthApplier {
        async fn apply(
            &self,
            request: reqwest::Request,
            target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            Ok(self
                .apply_with_authority(request, target)
                .await?
                .into_request())
        }

        async fn apply_with_authority(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<AppliedAuth> {
            match self.0 {
                bitrouter_sdk::language_model::types::AuthScheme::Bearer => {
                    request.headers_mut().insert(
                        reqwest::header::AUTHORIZATION,
                        reqwest::header::HeaderValue::from_static("Bearer same-principal"),
                    );
                }
                bitrouter_sdk::language_model::types::AuthScheme::XApiKey => {
                    request.headers_mut().insert(
                        "x-api-key",
                        reqwest::header::HeaderValue::from_static("same-principal"),
                    );
                }
            }
            Ok(AppliedAuth::proven_with_scheme(
                request,
                CredentialAuthority::derive("test/same-principal", "principal"),
                self.0,
            ))
        }

        async fn continuation_authority(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<CredentialAuthority>> {
            Ok(Some(CredentialAuthority::derive(
                "test/same-principal",
                "principal",
            )))
        }

        async fn continuation_authority_proof(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<ContinuationAuthority>> {
            Ok(Some(ContinuationAuthority::new(
                CredentialAuthority::derive("test/same-principal", "principal"),
                self.0,
            )))
        }
    }

    struct HeaderAuthApplier {
        headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
        route_scheme: bitrouter_sdk::language_model::types::AuthScheme,
    }

    #[async_trait]
    impl AuthApplier for HeaderAuthApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            for (name, value) in &self.headers {
                request.headers_mut().append(name, value.clone());
            }
            Ok(request)
        }

        async fn apply_with_authority(
            &self,
            request: reqwest::Request,
            target: &RoutingTarget,
        ) -> PipelineResult<AppliedAuth> {
            Ok(AppliedAuth::proven(
                self.apply(request, target).await?,
                CredentialAuthority::derive("test/header-principal", "principal"),
            ))
        }

        async fn continuation_authority_proof(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<ContinuationAuthority>> {
            Ok(Some(ContinuationAuthority::new(
                CredentialAuthority::derive("test/header-principal", "principal"),
                self.route_scheme,
            )))
        }
    }

    fn dynamic_auth_pipeline(
        registry: ContinuationRegistry,
        upstream: &MockServer,
        applier: Arc<dyn AuthApplier>,
    ) -> anyhow::Result<Arc<Pipeline>> {
        let mut upstream_target = target("static-placeholder");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let auth = AuthAppliers::new().with("openai", applier);
        let runtime = ContinuationRuntime::with_auth_appliers(registry, auth.clone());
        let executor =
            HttpExecutor::with_dispatch_and_auth(Default::default(), Default::default(), auth)?;
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(executor))
            .route_hook(runtime.clone())
            .required_finalizer(runtime);
        Ok(Arc::new(builder.build()?))
    }

    fn continuation_pipeline(
        registry: ContinuationRegistry,
        targets: Vec<RoutingTarget>,
    ) -> anyhow::Result<Arc<Pipeline>> {
        let runtime = ContinuationRuntime::new(registry);
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", targets);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime);
        Ok(Arc::new(builder.build()?))
    }

    async fn assert_duplicate_identity_fallback_binds_exact_second_target(
        stream: bool,
    ) -> anyhow::Result<()> {
        let first = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(500).set_body_string("first hop failed"))
            .mount(&first)
            .await;
        let second = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ContinuationResponder {
                state: state.clone(),
                stream,
            })
            .mount(&second)
            .await;

        let mut first_target = target("first-credential");
        first_target.api_base = first.uri();
        let mut second_target = target("second-credential");
        second_target.api_base = second.uri();
        assert_eq!(first_target.provider_name, second_target.provider_name);
        assert_eq!(first_target.service_id, second_target.service_id);
        assert_eq!(first_target.account_label, second_target.account_label);

        let registry = registry(if stream { 35 } else { 34 }).await?;
        let request_id = if stream {
            "duplicate-fallback-stream"
        } else {
            "duplicate-fallback-nonstream"
        };
        let pipeline = continuation_pipeline(
            registry.clone(),
            vec![first_target.clone(), second_target.clone()],
        )?;
        if stream {
            drain_stream(pipeline, tool_request(request_id, None)).await?;
        } else {
            pipeline
                .execute(nonstream_tool_request(request_id, None))
                .await?;
        }

        let public_id = encode_gateway_continuation_id(request_id)?;
        let ContinuationResolution::Active(active) = registry
            .resolve("tool-owner", &public_id, Utc::now())
            .await?
        else {
            anyhow::bail!("fallback continuation missing")
        };
        assert!(active.matches_target(&second_target, &static_authority("second-credential"))?);
        assert!(!active.matches_target(&first_target, &static_authority("first-credential"))?);

        let restarted =
            continuation_pipeline(registry, vec![first_target.clone(), second_target.clone()])?;
        if stream {
            drain_stream(
                restarted,
                tool_request("duplicate-fallback-resume", Some(&public_id)),
            )
            .await?;
        } else {
            restarted
                .execute(nonstream_tool_request(
                    "duplicate-fallback-resume",
                    Some(&public_id),
                ))
                .await?;
        }
        assert_eq!(
            first
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("request recording disabled"))?
                .len(),
            1,
            "restart resume must not retry the tuple-identical first target"
        );
        let state = state.lock().expect("continuation state poisoned");
        assert_eq!(state.calls, 2);
        assert_eq!(
            state.forwarded_parents,
            [None, Some("fallback-provider-root".into())]
        );
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_duplicate_identity_fallback_binds_exact_second_target() -> anyhow::Result<()>
    {
        assert_duplicate_identity_fallback_binds_exact_second_target(false).await
    }

    #[tokio::test]
    async fn stream_duplicate_identity_fallback_binds_exact_second_target() -> anyhow::Result<()> {
        assert_duplicate_identity_fallback_binds_exact_second_target(true).await
    }

    async fn drain_stream(pipeline: Arc<Pipeline>, request: PipelineRequest) -> PipelineResult<()> {
        let mut stream = pipeline.execute_stream(request).await?;
        while let Some(part) = stream.next().await {
            part?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn real_http_responses_terminal_followed_by_event_fails_without_binding()
    -> anyhow::Result<()> {
        for (index, trailing_frame) in [
            format!(
                "event: response.completed\ndata: {}",
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "provider-http-lifecycle",
                        "status": "completed",
                        "output": []
                    }
                })
            ),
            format!(
                "event: response.some_future_event\ndata: {}",
                serde_json::json!({"type": "response.some_future_event"})
            ),
            "data: [DONE]".to_string(),
            "data: malformed nonempty trailing data".to_string(),
        ]
        .into_iter()
        .enumerate()
        {
            let upstream = MockServer::start().await;
            let events = [
                serde_json::json!({
                    "type": "response.created",
                    "response": {
                        "id": "provider-http-lifecycle",
                        "status": "in_progress"
                    }
                }),
                serde_json::json!({
                    "type": "response.some_future_event_before_terminal"
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "provider-http-lifecycle",
                        "status": "completed",
                        "output": []
                    }
                }),
            ];
            let mut body = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().expect("event type")
                    )
                })
                .collect::<String>();
            body.push_str(&trailing_frame);
            body.push_str("\n\n");
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .mount(&upstream)
                .await;
            let registry = registry(57 + index as u8).await?;
            let mut upstream_target = target("credential");
            upstream_target.api_base = upstream.uri();
            let pipeline = continuation_pipeline(registry.clone(), vec![upstream_target])?;
            let request_id = format!("http-post-terminal-{index}");
            let mut stream = pipeline
                .clone()
                .execute_stream(tool_request(&request_id, None))
                .await?;
            let mut saw_error = false;
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    saw_error = true;
                }
            }
            pipeline.drain_pending_settlements().await;

            assert!(
                saw_error,
                "HttpExecutor stopped consuming immediately after the apparent terminal"
            );
            assert_eq!(
                registry
                    .resolve(
                        "tool-owner",
                        &encode_gateway_continuation_id(&request_id)?,
                        Utc::now(),
                    )
                    .await?,
                ContinuationResolution::Missing,
                "an invalid real HTTP lifecycle published a continuation"
            );
        }
        Ok(())
    }

    struct SearchTool;

    #[async_trait]
    impl bitrouter_sdk::language_model::server_tools::toolset::RouterToolset for SearchTool {
        async fn list_tools(
            &self,
            _ctx: &bitrouter_sdk::language_model::server_tools::toolset::ToolContext,
        ) -> PipelineResult<Vec<Tool>> {
            Ok(vec![Tool::Function {
                name: "search".into(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
                provider_metadata: Default::default(),
            }])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &str,
            _ctx: &bitrouter_sdk::language_model::server_tools::toolset::ToolContext,
        ) -> PipelineResult<ToolResultOutput> {
            Ok(ToolResultOutput::Text {
                value: "search-result".into(),
            })
        }

        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
    }

    struct FailingSearchTool;

    #[async_trait]
    impl bitrouter_sdk::language_model::server_tools::toolset::RouterToolset for FailingSearchTool {
        async fn list_tools(
            &self,
            _ctx: &bitrouter_sdk::language_model::server_tools::toolset::ToolContext,
        ) -> PipelineResult<Vec<Tool>> {
            Ok(vec![Tool::Function {
                name: "search".into(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
                provider_metadata: Default::default(),
            }])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &str,
            _ctx: &bitrouter_sdk::language_model::server_tools::toolset::ToolContext,
        ) -> PipelineResult<ToolResultOutput> {
            Err(BitrouterError::internal("synthetic search failure"))
        }

        fn owns(&self, name: &str) -> bool {
            name == "search"
        }
    }

    fn tool_request_with_stream(
        request_id: &str,
        previous_response_id: Option<&str>,
        stream: bool,
    ) -> PipelineRequest {
        let mut params = GenerationParams::default();
        if let Some(previous_response_id) = previous_response_id {
            params.extra.insert(
                "previous_response_id".into(),
                serde_json::Value::String(previous_response_id.into()),
            );
        }
        PipelineRequest {
            request_id: request_id.into(),
            model: "gpt-5".into(),
            caller: CallerContext::new("key", "tool-owner"),
            headers: Default::default(),
            prompt: Prompt {
                model: "gpt-5".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "answer with search")],
                tools: Vec::new(),
                params,
                response_format: None,
                tool_choice: None,
                stream,
            },
            inbound_protocol: Some(ApiProtocol::Responses),
        }
    }

    fn tool_request(request_id: &str, previous_response_id: Option<&str>) -> PipelineRequest {
        tool_request_with_stream(request_id, previous_response_id, true)
    }

    fn nonstream_tool_request(
        request_id: &str,
        previous_response_id: Option<&str>,
    ) -> PipelineRequest {
        tool_request_with_stream(request_id, previous_response_id, false)
    }

    fn nonstream_server_tool_pipeline(
        registry: ContinuationRegistry,
        upstream: &MockServer,
        config: bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig,
        tool: Arc<dyn bitrouter_sdk::language_model::server_tools::toolset::RouterToolset>,
    ) -> anyhow::Result<Arc<Pipeline>> {
        let runtime = ContinuationRuntime::new(registry);
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let server_loop = Arc::new(
            bitrouter_sdk::language_model::server_tools::loop_controller::ServerToolLoop::new(
                bitrouter_sdk::language_model::server_tools::toolset::ToolsetRegistry::new(vec![
                    tool,
                ]),
                config,
                Arc::new(bitrouter_sdk::language_model::server_tools::approval::AllowAll),
            ),
        );
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .server_tool_loop(server_loop);
        Ok(Arc::new(builder.build()?))
    }

    struct ErrorSettlementRecorder(Arc<std::sync::Mutex<Vec<bool>>>);

    #[async_trait]
    impl SettlementRecorder for ErrorSettlementRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> PipelineResult<()> {
            self.0
                .lock()
                .expect("settlement state poisoned")
                .push(ctx.error.is_some());
            Ok(())
        }
    }

    async fn assert_invalid_nonstream_responses_terminal_is_not_resumable(
        request_id: &str,
        body: serde_json::Value,
        expected_error: &str,
    ) -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
        let registry = registry(30).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let settlements = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(ErrorSettlementRecorder(settlements.clone()));
        let pipeline = Arc::new(builder.build()?);

        let error = pipeline
            .clone()
            .execute(nonstream_tool_request(request_id, None))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected_error),
            "unexpected upstream failure: {error}"
        );
        assert_eq!(
            settlements
                .lock()
                .expect("settlement state poisoned")
                .as_slice(),
            &[true],
            "invalid provider terminal must settle as failure"
        );
        let public_id = encode_gateway_continuation_id(request_id)?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing
        );
        let error = pipeline
            .execute(nonstream_tool_request(
                "invalid-terminal-resume",
                Some(&public_id),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mapping is unavailable"));
        assert_eq!(
            upstream
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("request recording disabled"))?
                .len(),
            1,
            "resume must reject before upstream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_missing_or_empty_responses_id_fails_closed() -> anyhow::Result<()> {
        for (suffix, id) in [("missing", None), ("empty", Some(""))] {
            let mut body = serde_json::json!({
                "status": "completed",
                "output": []
            });
            if let Some(id) = id {
                body["id"] = serde_json::Value::String(id.into());
            }
            assert_invalid_nonstream_responses_terminal_is_not_resumable(
                &format!("invalid-id-{suffix}"),
                body,
                "missing non-empty 'id'",
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_failed_or_unknown_responses_status_fails_closed() -> anyhow::Result<()> {
        for status in ["failed", "future-status"] {
            assert_invalid_nonstream_responses_terminal_is_not_resumable(
                &format!("invalid-status-{status}"),
                serde_json::json!({
                    "id": format!("provider-{status}"),
                    "status": status,
                    "output": []
                }),
                "non-success terminal status",
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn server_tool_continuation_binds_only_final_provider_round() -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ToolRoundResponder(state.clone()))
            .mount(&upstream)
            .await;

        let registry = registry(16).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let server_loop = Arc::new(
            bitrouter_sdk::language_model::server_tools::loop_controller::ServerToolLoop::new(
                bitrouter_sdk::language_model::server_tools::toolset::ToolsetRegistry::new(vec![
                    Arc::new(SearchTool),
                ]),
                bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig::default(
                ),
                Arc::new(bitrouter_sdk::language_model::server_tools::approval::AllowAll),
            ),
        );
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .server_tool_loop(server_loop);
        let pipeline = Arc::new(builder.build()?);

        let mut first = pipeline
            .clone()
            .execute_stream(tool_request("tool-request-1", None))
            .await?;
        let mut saw_tool = false;
        while let Some(part) = first.next().await {
            saw_tool |= matches!(part?, StreamPart::ServerToolCall { .. });
        }
        assert!(
            saw_tool,
            "the server tool loop did not execute its tool round"
        );

        let public_id = encode_gateway_continuation_id("tool-request-1")?;
        let resolved = registry
            .resolve("tool-owner", &public_id, Utc::now())
            .await?;
        let ContinuationResolution::Active(active) = resolved else {
            anyhow::bail!("final provider round was not published")
        };
        assert_eq!(active.provider_response_id, "provider-final");
        assert_ne!(active.provider_response_id, "provider-intermediate");

        let mut next = pipeline
            .execute_stream(tool_request("tool-request-2", Some(&public_id)))
            .await?;
        while let Some(part) = next.next().await {
            part?;
        }
        let state = state.lock().expect("tool round state poisoned");
        assert_eq!(
            state.forwarded_parents,
            [None, None, Some("provider-final".into())],
            "only the final provider round may be used for the next continuation"
        );
        assert_eq!(state.calls, 3);
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_server_tool_continuation_binds_only_final_provider_round()
    -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(NonstreamToolRoundResponder(state.clone()))
            .mount(&upstream)
            .await;

        let registry = registry(28).await?;
        let pipeline = nonstream_server_tool_pipeline(
            registry.clone(),
            &upstream,
            Default::default(),
            Arc::new(SearchTool),
        )?;
        let first = pipeline
            .clone()
            .execute(nonstream_tool_request("nonstream-tool-request-1", None))
            .await?;
        assert_eq!(first.result.response_id.as_deref(), Some("provider-final"));
        let usage = first
            .result
            .usage
            .ok_or_else(|| anyhow::anyhow!("aggregated usage missing"))?;
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (30, 5));

        let public_id = encode_gateway_continuation_id("nonstream-tool-request-1")?;
        let ContinuationResolution::Active(active) = registry
            .resolve("tool-owner", &public_id, Utc::now())
            .await?
        else {
            anyhow::bail!("final provider round was not published")
        };
        assert_eq!(active.provider_response_id, "provider-final");

        pipeline
            .execute(nonstream_tool_request(
                "nonstream-tool-request-2",
                Some(&public_id),
            ))
            .await?;
        let state = state.lock().expect("tool round state poisoned");
        assert_eq!(
            state.forwarded_parents,
            [None, None, Some("provider-final".into())]
        );
        assert_eq!(state.calls, 3);
        Ok(())
    }

    async fn assert_nonstream_synthetic_server_tool_terminal_is_not_resumable(
        request_id: &str,
        expected_reason: &str,
        config: bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig,
        tool: Arc<dyn bitrouter_sdk::language_model::server_tools::toolset::RouterToolset>,
    ) -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(NonstreamToolRoundResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(29).await?;
        let pipeline = nonstream_server_tool_pipeline(registry.clone(), &upstream, config, tool)?;

        let result = pipeline
            .clone()
            .execute(nonstream_tool_request(request_id, None))
            .await?;
        assert_eq!(
            result.result.finish_reason,
            Some(bitrouter_sdk::language_model::FinishReason::Other(
                expected_reason.into()
            ))
        );
        assert_eq!(
            result.result.response_id.as_deref(),
            Some("provider-intermediate"),
            "native id remains available for response framing and request-local audit"
        );
        let usage = result
            .result
            .usage
            .ok_or_else(|| anyhow::anyhow!("synthetic result usage missing"))?;
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (10, 2));

        let public_id = encode_gateway_continuation_id(request_id)?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "router-synthetic nonstream termination must not publish an intermediate provider id"
        );
        let error = pipeline
            .execute(nonstream_tool_request(
                "nonstream-synthetic-resume",
                Some(&public_id),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mapping is unavailable"));
        let state = state.lock().expect("tool round state poisoned");
        assert_eq!(
            state.calls, 1,
            "resume must fail before forwarding a stale native id upstream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_max_tool_iterations_does_not_publish_intermediate_continuation()
    -> anyhow::Result<()> {
        assert_nonstream_synthetic_server_tool_terminal_is_not_resumable(
            "nonstream-max-tool-request",
            "max_tool_iterations",
            bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig {
                max_iterations: 0,
                ..Default::default()
            },
            Arc::new(SearchTool),
        )
        .await
    }

    #[tokio::test]
    async fn nonstream_tool_errors_do_not_publish_intermediate_continuation() -> anyhow::Result<()>
    {
        assert_nonstream_synthetic_server_tool_terminal_is_not_resumable(
            "nonstream-tool-error-request",
            "tool_errors",
            bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig {
                max_consecutive_errors: 1,
                ..Default::default()
            },
            Arc::new(FailingSearchTool),
        )
        .await
    }

    async fn assert_synthetic_server_tool_terminal_is_not_resumable(
        request_id: &str,
        expected_reason: &str,
        config: bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig,
        tool: Arc<dyn bitrouter_sdk::language_model::server_tools::toolset::RouterToolset>,
    ) -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ToolRoundResponder(state.clone()))
            .mount(&upstream)
            .await;

        let registry = registry(18).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let server_loop = Arc::new(
            bitrouter_sdk::language_model::server_tools::loop_controller::ServerToolLoop::new(
                bitrouter_sdk::language_model::server_tools::toolset::ToolsetRegistry::new(vec![
                    tool,
                ]),
                config,
                Arc::new(bitrouter_sdk::language_model::server_tools::approval::AllowAll),
            ),
        );
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .server_tool_loop(server_loop);
        let pipeline = Arc::new(builder.build()?);

        let mut first = pipeline
            .clone()
            .execute_stream(tool_request(request_id, None))
            .await?;
        let mut terminal_reason = None;
        while let Some(part) = first.next().await {
            if let StreamPart::Finish {
                reason: bitrouter_sdk::language_model::FinishReason::Other(reason),
            } = part?
            {
                terminal_reason = Some(reason);
            }
        }
        assert_eq!(terminal_reason.as_deref(), Some(expected_reason));

        let public_id = encode_gateway_continuation_id(request_id)?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "router-synthetic termination must not publish an intermediate provider id"
        );
        let resume = pipeline
            .execute_stream(tool_request("synthetic-resume", Some(&public_id)))
            .await;
        let error = match resume {
            Ok(_) => anyhow::bail!("synthetic public id unexpectedly became resumable"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("mapping is unavailable"));
        let state = state.lock().expect("tool round state poisoned");
        assert_eq!(
            state.calls, 1,
            "resume must fail before forwarding a stale native id upstream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn max_tool_iterations_does_not_publish_intermediate_continuation() -> anyhow::Result<()>
    {
        let config = bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig {
            max_iterations: 0,
            ..Default::default()
        };
        assert_synthetic_server_tool_terminal_is_not_resumable(
            "max-tool-request",
            "max_tool_iterations",
            config,
            Arc::new(SearchTool),
        )
        .await
    }

    #[tokio::test]
    async fn tool_errors_do_not_publish_intermediate_continuation() -> anyhow::Result<()> {
        let config = bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig {
            max_consecutive_errors: 1,
            ..Default::default()
        };
        assert_synthetic_server_tool_terminal_is_not_resumable(
            "tool-error-request",
            "tool_errors",
            config,
            Arc::new(FailingSearchTool),
        )
        .await
    }

    #[tokio::test]
    async fn unchanged_dynamic_authority_resumes_across_restart() -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(20).await?;

        drain_stream(
            dynamic_auth_pipeline(
                registry.clone(),
                &upstream,
                Arc::new(DynamicAuthApplier {
                    credential: "principal-a".into(),
                }),
            )?,
            tool_request("dynamic-root", None),
        )
        .await?;
        let public_id = encode_gateway_continuation_id("dynamic-root")?;
        drain_stream(
            dynamic_auth_pipeline(
                registry,
                &upstream,
                Arc::new(DynamicAuthApplier {
                    credential: "principal-a".into(),
                }),
            )?,
            tool_request("dynamic-resume", Some(&public_id)),
        )
        .await?;

        let state = state.lock().expect("dynamic auth state poisoned");
        assert_eq!(
            state.calls.as_slice(),
            [
                (Some("Bearer principal-a".into()), None),
                (
                    Some("Bearer principal-a".into()),
                    Some("dynamic-provider-root".into())
                ),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn same_principal_with_changed_effective_scheme_rejects_before_upstream()
    -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(36).await?;

        drain_stream(
            dynamic_auth_pipeline(
                registry.clone(),
                &upstream,
                Arc::new(SchemeAuthApplier(
                    bitrouter_sdk::language_model::types::AuthScheme::Bearer,
                )),
            )?,
            tool_request("scheme-root", None),
        )
        .await?;
        let public_id = encode_gateway_continuation_id("scheme-root")?;

        let error = dynamic_auth_pipeline(
            registry,
            &upstream,
            Arc::new(SchemeAuthApplier(
                bitrouter_sdk::language_model::types::AuthScheme::XApiKey,
            )),
        )?
        .execute_stream(tool_request("scheme-resume", Some(&public_id)))
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("effective auth scheme change unexpectedly resumed"))?;
        assert!(error.to_string().contains("unavailable or changed"));
        assert_eq!(
            state
                .lock()
                .expect("dynamic auth state poisoned")
                .calls
                .len(),
            1,
            "scheme mismatch must reject before upstream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn malformed_or_ambiguous_wire_auth_fails_before_responses_dispatch() -> anyhow::Result<()>
    {
        use bitrouter_sdk::language_model::types::AuthScheme;
        use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(56).await?;
        let mut bound_target = target("static-placeholder");
        bound_target.api_base = upstream.uri();
        let authority = |scheme| {
            ContinuationAuthority::new(
                CredentialAuthority::derive("test/header-principal", "principal"),
                scheme,
            )
        };
        let authorization = || HeaderName::from_static("authorization");
        let x_api_key = || HeaderName::from_static("x-api-key");
        let x_goog_key = || HeaderName::from_static("x-goog-api-key");
        let invalid_cases = vec![
            (
                "basic",
                AuthScheme::Bearer,
                vec![(authorization(), HeaderValue::from_static("Basic secret"))],
            ),
            (
                "aws4",
                AuthScheme::Bearer,
                vec![(
                    authorization(),
                    HeaderValue::from_static("AWS4-HMAC-SHA256 credential"),
                )],
            ),
            (
                "empty-bearer",
                AuthScheme::Bearer,
                vec![(authorization(), HeaderValue::from_static("Bearer "))],
            ),
            (
                "non-utf8-bearer",
                AuthScheme::Bearer,
                vec![(authorization(), HeaderValue::from_bytes(b"Bearer \xff")?)],
            ),
            (
                "duplicate-authorization",
                AuthScheme::Bearer,
                vec![
                    (authorization(), HeaderValue::from_static("Bearer one")),
                    (authorization(), HeaderValue::from_static("Bearer two")),
                ],
            ),
            (
                "bearer-plus-x-key",
                AuthScheme::Bearer,
                vec![
                    (authorization(), HeaderValue::from_static("Bearer secret")),
                    (x_api_key(), HeaderValue::from_static("secret")),
                ],
            ),
            (
                "two-x-key-families",
                AuthScheme::XApiKey,
                vec![
                    (x_api_key(), HeaderValue::from_static("secret")),
                    (x_goog_key(), HeaderValue::from_static("secret")),
                ],
            ),
        ];

        for (label, scheme, headers) in invalid_cases {
            let root_id = encode_gateway_continuation_id(&format!("wire-auth-root-{label}"))?;
            registry
                .bind(
                    "tool-owner",
                    &root_id,
                    "provider-wire-auth-root",
                    &bound_target,
                    &authority(scheme),
                    Utc::now(),
                )
                .await?;
            let before = state
                .lock()
                .expect("dynamic auth state poisoned")
                .calls
                .len();
            let result = dynamic_auth_pipeline(
                registry.clone(),
                &upstream,
                Arc::new(HeaderAuthApplier {
                    headers,
                    route_scheme: scheme,
                }),
            )?
            .execute_stream(tool_request(
                &format!("wire-auth-resume-{label}"),
                Some(&root_id),
            ))
            .await;
            assert!(
                result.is_err(),
                "{label} produced a continuation authority and opened an upstream stream"
            );
            assert_eq!(
                state
                    .lock()
                    .expect("dynamic auth state poisoned")
                    .calls
                    .len(),
                before,
                "{label} reached upstream before failing closed"
            );
        }

        for (label, scheme, headers) in [
            (
                "bearer-control",
                AuthScheme::Bearer,
                vec![(AUTHORIZATION, HeaderValue::from_static("bEaReR secret"))],
            ),
            (
                "x-key-control",
                AuthScheme::XApiKey,
                vec![(x_api_key(), HeaderValue::from_static("secret"))],
            ),
        ] {
            let root_id = encode_gateway_continuation_id(&format!("wire-auth-root-{label}"))?;
            registry
                .bind(
                    "tool-owner",
                    &root_id,
                    "provider-wire-auth-root",
                    &bound_target,
                    &authority(scheme),
                    Utc::now(),
                )
                .await?;
            drain_stream(
                dynamic_auth_pipeline(
                    registry.clone(),
                    &upstream,
                    Arc::new(HeaderAuthApplier {
                        headers,
                        route_scheme: scheme,
                    }),
                )?,
                tool_request(&format!("wire-auth-resume-{label}"), Some(&root_id)),
            )
            .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn malicious_observe_trace_headers_cannot_replace_final_wire_auth() -> anyhow::Result<()>
    {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(ToolRoundState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ContinuationResponder {
                state: state.clone(),
                stream: true,
            })
            .mount(&upstream)
            .await;
        let registry = registry(63).await?;
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();

        drain_stream(
            continuation_pipeline(registry.clone(), vec![upstream_target.clone()])?,
            tool_request("trace-mutation-root", None),
        )
        .await?;
        let public_id = encode_gateway_continuation_id("trace-mutation-root")?;

        let runtime = ContinuationRuntime::new(registry);
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .observe_hook(MaliciousTraceHeaderObserver);
        drain_stream(
            Arc::new(builder.build()?),
            tool_request("trace-mutation-resume", Some(&public_id)),
        )
        .await?;

        let requests = upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("request recording disabled"))?;
        let resumed = requests
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("resume request was not dispatched"))?;
        assert_eq!(
            resumed
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer credential"),
            "an observer replaced the authenticated transport credential"
        );
        assert!(!resumed.headers.contains_key("x-api-key"));
        assert!(!resumed.headers.contains_key("x-goog-api-key"));
        assert_eq!(
            resumed
                .headers
                .get("x-bitrouter-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("trace-mutation-resume")
        );
        assert_eq!(
            resumed
                .headers
                .get("traceparent")
                .and_then(|value| value.to_str().ok()),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
        assert_eq!(
            resumed
                .headers
                .get("tracestate")
                .and_then(|value| value.to_str().ok()),
            Some("vendor=value")
        );
        let body: serde_json::Value = serde_json::from_slice(&resumed.body)?;
        assert_eq!(
            body.get("previous_response_id")
                .and_then(serde_json::Value::as_str),
            Some("fallback-provider-root")
        );
        Ok(())
    }

    #[tokio::test]
    async fn replaced_dynamic_authority_rejects_before_upstream() -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(21).await?;
        drain_stream(
            dynamic_auth_pipeline(
                registry.clone(),
                &upstream,
                Arc::new(DynamicAuthApplier {
                    credential: "principal-a".into(),
                }),
            )?,
            tool_request("dynamic-replaced-root", None),
        )
        .await?;
        let public_id = encode_gateway_continuation_id("dynamic-replaced-root")?;

        let result = dynamic_auth_pipeline(
            registry,
            &upstream,
            Arc::new(DynamicAuthApplier {
                credential: "principal-b".into(),
            }),
        )?
        .execute_stream(tool_request("dynamic-replaced-resume", Some(&public_id)))
        .await;
        let error = match result {
            Ok(_) => anyhow::bail!("replaced dynamic authority reached upstream"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unavailable or changed"));
        let state = state.lock().expect("dynamic auth state poisoned");
        assert_eq!(
            state.calls.len(),
            1,
            "replacement must fail before upstream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_dynamic_authority_never_publishes_resumable_root() -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state))
            .mount(&upstream)
            .await;
        let registry = registry(22).await?;
        let pipeline = dynamic_auth_pipeline(
            registry.clone(),
            &upstream,
            Arc::new(UnsupportedDynamicAuthApplier),
        )?;
        let result = pipeline
            .execute_stream(tool_request("unsupported-dynamic-root", None))
            .await;
        let failure = match result {
            Ok(_) => anyhow::bail!("unsupported dynamic authority reached upstream"),
            Err(error) => error,
        };
        assert!(
            failure
                .to_string()
                .contains("continuation authority unavailable")
        );
        assert_eq!(
            upstream
                .received_requests()
                .await
                .map_or(0, |requests| requests.len()),
            0,
            "unsupported authority must fail before upstream"
        );
        assert_eq!(
            registry
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id("unsupported-dynamic-root")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn dynamic_authority_race_rejects_before_upstream_dispatch() -> anyhow::Result<()> {
        let upstream = MockServer::start().await;
        let state = Arc::new(std::sync::Mutex::new(DynamicAuthState::default()));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(DynamicAuthResponder(state.clone()))
            .mount(&upstream)
            .await;
        let registry = registry(23).await?;
        drain_stream(
            dynamic_auth_pipeline(
                registry.clone(),
                &upstream,
                Arc::new(DynamicAuthApplier {
                    credential: "principal-a".into(),
                }),
            )?,
            tool_request("dynamic-race-root", None),
        )
        .await?;
        let public_id = encode_gateway_continuation_id("dynamic-race-root")?;

        let result = dynamic_auth_pipeline(registry, &upstream, Arc::new(RacedDynamicAuthApplier))?
            .execute_stream(tool_request("dynamic-race-resume", Some(&public_id)))
            .await;
        let error = match result {
            Ok(_) => anyhow::bail!("raced dynamic authority reached upstream"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("credential authority changed"));
        assert_eq!(
            state
                .lock()
                .expect("dynamic auth state poisoned")
                .calls
                .len(),
            1,
            "authority race must fail before upstream"
        );
        Ok(())
    }

    async fn assert_finalizer_failure_precedes_terminal(
        registry: ContinuationRegistry,
        expected_error: &str,
    ) -> anyhow::Result<()> {
        let runtime = ContinuationRuntime::new(registry);
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ResponseStarted {
                id: "provider-final".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::TextDelta {
                text: "not yet successful".into(),
            },
            StreamPart::ResponseCompleted {
                id: "provider-final".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            },
        ])]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .required_finalizer(runtime);
        let pipeline = Arc::new(builder.build()?);
        let mut stream = pipeline
            .execute_stream(tool_request("required-finalizer-failure", None))
            .await?;
        let mut saw_success_terminal = false;
        let mut failure = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. }) => {
                    saw_success_terminal = true;
                }
                Ok(_) => {}
                Err(error) => failure = Some(error),
            }
        }
        assert!(!saw_success_terminal);
        let failure = failure.ok_or_else(|| anyhow::anyhow!("required finalizer did not fail"))?;
        assert!(
            failure.to_string().contains(expected_error),
            "unexpected finalizer error: {failure}"
        );
        Ok(())
    }

    struct BlockingRequiredFinalizer {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<AtomicBool>,
    }

    struct LateCompletingContinuationFinalizer {
        runtime: ContinuationRuntime,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<tokio::sync::Notify>,
    }

    struct DuplicateRollbackBarrier {
        request_id: String,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct CollidingAttemptFinalizer {
        runtime: ContinuationRuntime,
        forced_delivery_attempt_id: u64,
        blocked_request_id: String,
        first_commit_started: Arc<tokio::sync::Notify>,
        release_first_commit: Arc<tokio::sync::Notify>,
        duplicate_rollback: Option<DuplicateRollbackBarrier>,
    }

    impl CollidingAttemptFinalizer {
        fn context(&self, context: &RequiredFinalizationContext) -> RequiredFinalizationContext {
            let mut context = context.clone();
            context.delivery_attempt_id = self.forced_delivery_attempt_id;
            context
        }
    }

    struct BlockingEndHook {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug)]
    enum TerminalMutation {
        Drop,
        ReplaceFailed,
        ReplaceNonterminal,
    }

    struct TerminalMutatingHook(TerminalMutation);

    #[async_trait]
    impl StreamHook for TerminalMutatingHook {
        fn interest(&self) -> StreamInterest {
            StreamInterest::none().with_finish()
        }

        async fn on_part(
            &self,
            _ctx: &mut StreamContext,
            part: StreamPart,
        ) -> PipelineResult<StreamAction> {
            Ok(match self.0 {
                TerminalMutation::Drop => StreamAction::Drop,
                TerminalMutation::ReplaceFailed => {
                    let id = match part {
                        StreamPart::ResponseCompleted { id, .. } => id,
                        _ => "replacement-id".into(),
                    };
                    StreamAction::Replace(vec![StreamPart::ResponseCompleted {
                        id,
                        source_protocol: ApiProtocol::Responses,
                        status: "failed".into(),
                        usage: None,
                        response_output_commitment: None,
                    }])
                }
                TerminalMutation::ReplaceNonterminal => {
                    StreamAction::Replace(vec![StreamPart::TextDelta {
                        text: "terminal removed".into(),
                    }])
                }
            })
        }

        async fn on_stream_end(
            &self,
            _ctx: &mut StreamContext,
            _outcome: &StreamOutcome,
        ) -> PipelineResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl StreamHook for BlockingEndHook {
        fn interest(&self) -> StreamInterest {
            StreamInterest::none()
        }

        async fn on_part(
            &self,
            _ctx: &mut StreamContext,
            _part: StreamPart,
        ) -> PipelineResult<StreamAction> {
            Ok(StreamAction::Pass)
        }

        async fn on_stream_end(
            &self,
            _ctx: &mut StreamContext,
            _outcome: &StreamOutcome,
        ) -> PipelineResult<()> {
            self.started.notify_one();
            self.release.notified().await;
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct UsageSettlementRecorder(Arc<std::sync::Mutex<Vec<(u64, u64)>>>);

    struct BlockingSettlementRecorder {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct NativeIdStealingRecorder(Arc<std::sync::Mutex<Vec<String>>>);

    struct EchoingResumeErrorResponder {
        calls: Arc<AtomicUsize>,
        native_id: &'static str,
        credential: &'static str,
    }

    struct RefreshingEchoErrorResponder {
        calls: Arc<AtomicUsize>,
        native_id: &'static str,
        old_credential: &'static str,
        new_credential: &'static str,
        old_encoded_credential: &'static str,
        new_encoded_credential: &'static str,
    }

    struct StreamingPlainEchoErrorResponder {
        calls: Arc<AtomicUsize>,
        native_id: &'static str,
        credential: &'static str,
    }

    struct StreamingSseEchoErrorResponder {
        calls: Arc<AtomicUsize>,
        native_id: &'static str,
        credential: &'static str,
    }

    impl Respond for StreamingSseEchoErrorResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let body = if call == 0 {
                [
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": self.native_id, "status": "in_progress"}
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"id": self.native_id, "status": "completed", "output": []}
                    }),
                ]
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap_or("response.event")
                    )
                })
                .collect::<String>()
            } else {
                let unknown_type = format!("future-{}-{}", self.native_id, self.credential);
                let unknown = serde_json::json!({"type": unknown_type});
                let event = serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "error": {
                            "type": "invalid_request_error",
                            "message": format!(
                                "failed parent={} credential={} repeated={}",
                                self.native_id, self.credential, self.native_id
                            )
                        }
                    }
                });
                format!(
                    "event: response.future\ndata: {unknown}\n\nevent: response.failed\ndata: {event}\n\n"
                )
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
        }
    }

    impl Respond for StreamingPlainEchoErrorResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let events = [
                    serde_json::json!({
                        "type": "response.created",
                        "response": {"id": self.native_id, "status": "in_progress"}
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {"id": self.native_id, "status": "completed", "output": []}
                    }),
                ];
                let body = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap_or("response.event")
                        )
                    })
                    .collect::<String>();
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body);
            }

            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let echoed_parent = body
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing-parent");
            ResponseTemplate::new(400).set_body_string(format!(
                "parent={echoed_parent}; credential={}; padding={}; repeated={echoed_parent}; credential={}",
                self.credential,
                "x".repeat(1_500),
                self.credential
            ))
        }
    }

    impl Respond for RefreshingEchoErrorResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": self.native_id,
                    "status": "completed",
                    "output": []
                }));
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let echoed_parent = body
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing-parent");
            if call == 1 {
                return ResponseTemplate::new(401).set_body_string(format!(
                    "expired {} for {echoed_parent}",
                    self.old_credential
                ));
            }
            ResponseTemplate::new(400).set_body_string(format!(
                "parent={echoed_parent}; old={}; new={}; old_raw={}; new_raw={}; repeated={echoed_parent}",
                self.old_credential,
                self.new_credential,
                self.old_encoded_credential,
                self.new_encoded_credential,
            ))
        }
    }

    struct RefreshingCustomAuthApplier {
        generation: Arc<AtomicUsize>,
        old_credential: &'static str,
        new_credential: &'static str,
    }

    impl RefreshingCustomAuthApplier {
        fn credential(&self) -> &'static str {
            if self.generation.load(Ordering::SeqCst) == 0 {
                self.old_credential
            } else {
                self.new_credential
            }
        }

        fn authority() -> ContinuationAuthority {
            ContinuationAuthority::new(
                CredentialAuthority::derive("test/refreshing-custom-auth", "stable-principal"),
                bitrouter_sdk::language_model::types::AuthScheme::Bearer,
            )
        }
    }

    #[async_trait]
    impl AuthApplier for RefreshingCustomAuthApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> PipelineResult<reqwest::Request> {
            let credential = reqwest::header::HeaderValue::from_static(self.credential());
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer stable-principal-proof"),
            );
            request.headers_mut().insert("x-provider-auth", credential);
            request
                .url_mut()
                .query_pairs_mut()
                .append_pair("access_token", self.credential());
            Ok(request)
        }

        async fn apply_with_authority(
            &self,
            request: reqwest::Request,
            target: &RoutingTarget,
        ) -> PipelineResult<AppliedAuth> {
            Ok(AppliedAuth::proven_with_scheme(
                self.apply(request, target).await?,
                Self::authority().credential().clone(),
                bitrouter_sdk::language_model::types::AuthScheme::Bearer,
            ))
        }

        async fn continuation_authority_proof(
            &self,
            _target: &RoutingTarget,
        ) -> PipelineResult<Option<ContinuationAuthority>> {
            Ok(Some(Self::authority()))
        }

        async fn refresh_after_unauthorized(
            &self,
            _target: &RoutingTarget,
            _rejected_authorization: Option<&reqwest::header::HeaderValue>,
        ) -> PipelineResult<bool> {
            self.generation.store(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    impl Respond for EchoingResumeErrorResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": self.native_id,
                    "status": "completed",
                    "output": []
                }));
            }

            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|error| serde_json::json!({"parse_error": error.to_string()}));
            let echoed_parent = body
                .get("previous_response_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing-parent");
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": format!(
                        "rejected {echoed_parent}; again={echoed_parent}; credential={}",
                        self.credential
                    ),
                    "nested": [echoed_parent, {"credential": self.credential}]
                }
            }))
        }
    }

    struct CapturedLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl SettlementRecorder for UsageSettlementRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> PipelineResult<()> {
            self.0
                .lock()
                .expect("usage settlement state poisoned")
                .push((ctx.prompt_tokens, ctx.completion_tokens));
            Ok(())
        }
    }

    #[async_trait]
    impl SettlementRecorder for BlockingSettlementRecorder {
        async fn record(&self, _ctx: &mut SettlementContext) -> PipelineResult<()> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[async_trait]
    impl SettlementRecorder for NativeIdStealingRecorder {
        async fn record(&self, ctx: &mut SettlementContext) -> PipelineResult<()> {
            let snapshot = format!(
                "request={};model={};provider={};account={:?};finish={:?};error={:?}",
                ctx.request_id,
                ctx.model_id,
                ctx.provider_id,
                ctx.account_label,
                ctx.finish_reason,
                ctx.error,
            );
            self.0
                .lock()
                .expect("malicious recorder state poisoned")
                .push(snapshot);
            Ok(())
        }
    }

    struct OutcomeObserver(Arc<std::sync::Mutex<Vec<&'static str>>>);

    struct MaliciousTraceHeaderObserver;

    #[async_trait]
    impl ObserveHook for MaliciousTraceHeaderObserver {
        async fn after_phase(
            &self,
            _phase: bitrouter_sdk::language_model::Phase,
            _ctx: &PipelineContext,
        ) {
        }

        async fn on_hop_start(&self, ctx: &PipelineContext, _target: &RoutingTarget) {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_static("Bearer observer-attacker"),
            );
            headers.insert(
                "x-api-key",
                http::HeaderValue::from_static("observer-attacker"),
            );
            headers.insert(
                "x-goog-api-key",
                http::HeaderValue::from_static("observer-attacker"),
            );
            headers.insert(
                "x-bitrouter-request-id",
                http::HeaderValue::from_static("observer-attacker"),
            );
            headers.insert(
                "traceparent",
                http::HeaderValue::from_static(
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                ),
            );
            headers.insert("tracestate", http::HeaderValue::from_static("vendor=value"));
            ctx.set_outbound_trace_headers(headers);
        }

        async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

        async fn on_request_end(&self, _ctx: &PipelineContext, _outcome: &RequestOutcome) {}
    }

    #[async_trait]
    impl ObserveHook for OutcomeObserver {
        async fn after_phase(
            &self,
            _phase: bitrouter_sdk::language_model::Phase,
            _ctx: &PipelineContext,
        ) {
        }

        async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

        async fn on_request_end(&self, _ctx: &PipelineContext, outcome: &RequestOutcome) {
            self.0
                .lock()
                .expect("outcome observer poisoned")
                .push(match outcome {
                    RequestOutcome::Completed => "completed",
                    RequestOutcome::Failed(_) => "failed",
                    RequestOutcome::ClientDisconnected => "disconnected",
                });
        }
    }

    #[async_trait]
    impl RequiredFinalizer for BlockingRequiredFinalizer {
        async fn finalize(&self, _ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
            self.started.notify_one();
            self.release.notified().await;
            self.completed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl RequiredFinalizer for CollidingAttemptFinalizer {
        async fn finalize(&self, context: &RequiredFinalizationContext) -> PipelineResult<()> {
            self.runtime.finalize(&self.context(context)).await
        }

        async fn finalize_with_receipt(
            &self,
            context: &RequiredFinalizationContext,
            receipt: &RequiredFinalizationReceipt,
        ) -> PipelineResult<()> {
            self.runtime
                .finalize_with_receipt(&self.context(context), receipt)
                .await
        }

        async fn rollback(&self, context: &RequiredFinalizationContext) -> PipelineResult<()> {
            if let Some(barrier) = self.duplicate_rollback.as_ref()
                && barrier.request_id == context.request_id
            {
                barrier.started.notify_one();
                barrier.release.notified().await;
            }
            self.runtime.rollback(&self.context(context)).await
        }

        async fn rollback_with_receipt(
            &self,
            context: &RequiredFinalizationContext,
            receipt: &RequiredFinalizationReceipt,
        ) -> PipelineResult<()> {
            if let Some(barrier) = self.duplicate_rollback.as_ref()
                && barrier.request_id == context.request_id
            {
                barrier.started.notify_one();
                barrier.release.notified().await;
            }
            self.runtime
                .rollback_with_receipt(&self.context(context), receipt)
                .await
        }

        async fn commit(
            &self,
            context: &RequiredFinalizationContext,
            delivery: &RequiredDeliveryHandshake,
        ) -> PipelineResult<bool> {
            if self.blocked_request_id == context.request_id {
                self.first_commit_started.notify_one();
                self.release_first_commit.notified().await;
            }
            self.runtime.commit(&self.context(context), delivery).await
        }

        async fn commit_with_receipt(
            &self,
            context: &RequiredFinalizationContext,
            receipt: &RequiredFinalizationReceipt,
            delivery: &RequiredDeliveryHandshake,
        ) -> PipelineResult<bool> {
            if self.blocked_request_id == context.request_id {
                self.first_commit_started.notify_one();
                self.release_first_commit.notified().await;
            }
            self.runtime
                .commit_with_receipt(&self.context(context), receipt, delivery)
                .await
        }
    }

    struct ComposedContinuationFinalizer {
        runtime: ContinuationRuntime,
        blocker: BlockingRequiredFinalizer,
        continuation_first: bool,
    }

    #[async_trait]
    impl RequiredFinalizer for ComposedContinuationFinalizer {
        async fn finalize(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
            if self.continuation_first {
                self.runtime.finalize(ctx).await?;
                self.blocker.finalize(ctx).await
            } else {
                self.blocker.finalize(ctx).await?;
                self.runtime.finalize(ctx).await
            }
        }

        async fn rollback(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
            self.runtime.rollback(ctx).await
        }

        async fn commit(
            &self,
            ctx: &RequiredFinalizationContext,
            delivery: &RequiredDeliveryHandshake,
        ) -> PipelineResult<bool> {
            self.runtime.commit(ctx, delivery).await
        }
    }

    #[async_trait]
    impl RequiredFinalizer for LateCompletingContinuationFinalizer {
        async fn finalize(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
            let runtime = self.runtime.clone();
            let ctx = ctx.clone();
            let release = self.release.clone();
            let completed = self.completed.clone();
            let worker = tokio::spawn(async move {
                release.notified().await;
                let result = runtime.finalize(&ctx).await;
                completed.notify_one();
                result
            });
            self.started.notify_one();
            worker.await.map_err(|error| {
                BitrouterError::internal(format!("late finalizer task: {error}"))
            })?
        }

        async fn rollback(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
            self.runtime.rollback(ctx).await
        }

        async fn commit(
            &self,
            ctx: &RequiredFinalizationContext,
            delivery: &RequiredDeliveryHandshake,
        ) -> PipelineResult<bool> {
            self.runtime.commit(ctx, delivery).await
        }
    }

    #[tokio::test]
    async fn ordinary_settlement_recorder_cannot_obtain_native_responses_id() -> anyhow::Result<()>
    {
        const NATIVE_SENTINEL: &str = "native-response-secret-sentinel";
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            GenerateResult {
                content: vec![Content::Text {
                    text: "ready".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: Some(NATIVE_SENTINEL.into()),
                stop_details: None,
                provider_metadata: Default::default(),
            },
        )]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .settlement_recorder(NativeIdStealingRecorder(captured.clone()));
        Arc::new(builder.build()?)
            .execute(nonstream_tool_request("privacy-recorder", None))
            .await?;

        let snapshot = captured
            .lock()
            .expect("malicious recorder state poisoned")
            .join("\n");
        assert!(
            !snapshot.contains(NATIVE_SENTINEL),
            "generic settlement recorder exfiltrated a native Responses id: {snapshot}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resumed_http_error_scrubs_native_parent_and_credential_before_public_surfaces()
    -> anyhow::Result<()> {
        const NATIVE_SENTINEL: &str = "native-resume-private-sentinel";
        const CREDENTIAL_SENTINEL: &str = "credential-private-sentinel";
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(EchoingResumeErrorResponder {
                calls: Arc::new(AtomicUsize::new(0)),
                native_id: NATIVE_SENTINEL,
                credential: CREDENTIAL_SENTINEL,
            })
            .mount(&upstream)
            .await;

        let registry = registry(63).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut upstream_target = target(CREDENTIAL_SENTINEL);
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let recorder_snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(NativeIdStealingRecorder(recorder_snapshots.clone()));
        let pipeline = Arc::new(builder.build()?);

        let captured_logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_sink = captured_logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || CapturedLogWriter(log_sink.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let root_request_id = "resume-error-privacy-root";
        pipeline
            .clone()
            .execute(nonstream_tool_request(root_request_id, None))
            .await?;
        let public_id = encode_gateway_continuation_id(root_request_id)?;
        let error = pipeline
            .clone()
            .execute(nonstream_tool_request(
                "resume-error-privacy-child",
                Some(&public_id),
            ))
            .await
            .expect_err("echoed resume error must fail");
        pipeline.drain_pending_settlements().await;

        let recorder_output = recorder_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        let outputs = [
            error.to_string(),
            recorder_output.clone(),
            String::from_utf8(
                captured_logs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )?,
        ];
        for output in outputs {
            assert!(
                !output.contains(NATIVE_SENTINEL),
                "native continuation leaked through a public error surface: {output}"
            );
            assert!(
                !output.contains(CREDENTIAL_SENTINEL),
                "credential leaked through a public error surface: {output}"
            );
        }
        assert!(
            recorder_output.contains(&public_id),
            "available public continuation handle was not substituted into the classified error: {recorder_output}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resumed_nonstream_public_http_json_scrubs_native_parent_and_credential()
    -> anyhow::Result<()> {
        const NATIVE_SENTINEL: &str = "native-public-http-private-sentinel";
        const CREDENTIAL_SENTINEL: &str = "credential-public-http-private-sentinel";
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(EchoingResumeErrorResponder {
                calls: Arc::new(AtomicUsize::new(0)),
                native_id: NATIVE_SENTINEL,
                credential: CREDENTIAL_SENTINEL,
            })
            .mount(&upstream)
            .await;

        let registry = registry(67).await?;
        let runtime = ContinuationRuntime::new(registry);
        let mut upstream_target = target(CREDENTIAL_SENTINEL);
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime);
        let pipeline = Arc::new(builder.build()?);
        let app = build_router(app_state(pipeline.clone()));

        let root_request_id = "resume-public-http-privacy-root";
        let root_response = app
            .clone()
            .oneshot(responses_http_request(root_request_id, false))
            .await?;
        assert_eq!(root_response.status(), axum::http::StatusCode::OK);
        let _ = axum::body::to_bytes(root_response.into_body(), usize::MAX).await?;
        pipeline.drain_pending_settlements().await;

        let public_id = encode_gateway_continuation_id(root_request_id)?;
        let resume_request = HttpRequest::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .header("x-bitrouter-request-id", "resume-public-http-privacy-child")
            .body(Body::from(
                serde_json::json!({
                    "model": "gpt-5",
                    "input": "continue",
                    "stream": false,
                    "previous_response_id": public_id.clone(),
                })
                .to_string(),
            ))?;
        let response = app.oneshot(resume_request).await?;
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await?
                .to_vec(),
        )?;

        assert!(!body.contains(NATIVE_SENTINEL), "native id leaked: {body}");
        assert!(
            !body.contains(CREDENTIAL_SENTINEL),
            "credential leaked: {body}"
        );
        assert!(
            body.contains(&public_id),
            "public continuation handle was not retained: {body}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resumed_http_error_scrubs_old_and_refreshed_dynamic_wire_credentials()
    -> anyhow::Result<()> {
        const NATIVE_SENTINEL: &str = "native-refresh-private-sentinel";
        const OLD_CREDENTIAL: &str = "old/refresh+private%credential";
        const NEW_CREDENTIAL: &str = "new/refresh+private%credential";
        const OLD_ENCODED_CREDENTIAL: &str = "old%2Frefresh%2Bprivate%25credential";
        const NEW_ENCODED_CREDENTIAL: &str = "new%2Frefresh%2Bprivate%25credential";
        let upstream = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(RefreshingEchoErrorResponder {
                calls: calls.clone(),
                native_id: NATIVE_SENTINEL,
                old_credential: OLD_CREDENTIAL,
                new_credential: NEW_CREDENTIAL,
                old_encoded_credential: OLD_ENCODED_CREDENTIAL,
                new_encoded_credential: NEW_ENCODED_CREDENTIAL,
            })
            .mount(&upstream)
            .await;

        let registry = registry(64).await?;
        let generation = Arc::new(AtomicUsize::new(0));
        let pipeline = dynamic_auth_pipeline(
            registry,
            &upstream,
            Arc::new(RefreshingCustomAuthApplier {
                generation,
                old_credential: OLD_CREDENTIAL,
                new_credential: NEW_CREDENTIAL,
            }),
        )?;
        let root_request_id = "resume-refresh-privacy-root";
        pipeline
            .clone()
            .execute(nonstream_tool_request(root_request_id, None))
            .await?;
        let public_id = encode_gateway_continuation_id(root_request_id)?;
        let error = pipeline
            .execute(nonstream_tool_request(
                "resume-refresh-privacy-child",
                Some(&public_id),
            ))
            .await
            .expect_err("refreshed request must surface its final upstream rejection");
        let classified = format!("{error:?}");

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        for sensitive in [
            NATIVE_SENTINEL,
            OLD_CREDENTIAL,
            NEW_CREDENTIAL,
            OLD_ENCODED_CREDENTIAL,
            NEW_ENCODED_CREDENTIAL,
        ] {
            assert!(
                !classified.contains(sensitive),
                "refreshed wire secret leaked through classified error: {classified}"
            );
        }
        assert!(
            classified.contains(&public_id),
            "native continuation was not replaced with the available public handle: {classified}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resumed_stream_http_plain_error_scrubs_before_bounded_classification()
    -> anyhow::Result<()> {
        const NATIVE_SENTINEL: &str = "native-stream-private-sentinel";
        const CREDENTIAL_SENTINEL: &str = "credential-stream-private-sentinel";
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(StreamingPlainEchoErrorResponder {
                calls: Arc::new(AtomicUsize::new(0)),
                native_id: NATIVE_SENTINEL,
                credential: CREDENTIAL_SENTINEL,
            })
            .mount(&upstream)
            .await;

        let registry = registry(65).await?;
        let runtime = ContinuationRuntime::new(registry);
        let mut upstream_target = target(CREDENTIAL_SENTINEL);
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let recorder_snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(NativeIdStealingRecorder(recorder_snapshots.clone()));
        let pipeline = Arc::new(builder.build()?);

        let root_request_id = "resume-stream-privacy-root";
        drain_stream(pipeline.clone(), tool_request(root_request_id, None)).await?;
        let public_id = encode_gateway_continuation_id(root_request_id)?;
        let error = match pipeline
            .clone()
            .execute_stream(tool_request(
                "resume-stream-privacy-child",
                Some(&public_id),
            ))
            .await
        {
            Err(error) => error,
            Ok(_) => anyhow::bail!("plain upstream rejection returned a stream"),
        };
        pipeline.drain_pending_settlements().await;
        let classified = format!("{error:?}");
        let recorder_output = recorder_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        for output in [&classified, &recorder_output] {
            assert!(
                !output.contains(NATIVE_SENTINEL),
                "native id leaked: {output}"
            );
            assert!(
                !output.contains(CREDENTIAL_SENTINEL),
                "credential leaked: {output}"
            );
        }
        assert!(classified.contains(&public_id));
        Ok(())
    }

    #[tokio::test]
    async fn resumed_stream_http_success_status_sse_error_scrubs_typed_decoder_error()
    -> anyhow::Result<()> {
        const NATIVE_SENTINEL: &str = "native-sse-error-private-sentinel";
        const CREDENTIAL_SENTINEL: &str = "credential-sse-error-private-sentinel";
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(StreamingSseEchoErrorResponder {
                calls: Arc::new(AtomicUsize::new(0)),
                native_id: NATIVE_SENTINEL,
                credential: CREDENTIAL_SENTINEL,
            })
            .mount(&upstream)
            .await;

        let registry = registry(66).await?;
        let runtime = ContinuationRuntime::new(registry);
        let mut upstream_target = target(CREDENTIAL_SENTINEL);
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let recorder_snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(NativeIdStealingRecorder(recorder_snapshots.clone()));
        let pipeline = Arc::new(builder.build()?);

        let captured_logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_sink = captured_logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || CapturedLogWriter(log_sink.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let root_request_id = "resume-sse-error-privacy-root";
        drain_stream(pipeline.clone(), tool_request(root_request_id, None)).await?;
        let public_id = encode_gateway_continuation_id(root_request_id)?;
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request(
                "resume-sse-error-privacy-child",
                Some(&public_id),
            ))
            .await?;
        let mut caller_errors = Vec::new();
        while let Some(part) = stream.next().await {
            if let Err(error) = part {
                caller_errors.push(format!("{error:?}"));
            }
        }
        pipeline.drain_pending_settlements().await;
        let recorder_output = recorder_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        let caller_output = caller_errors.join("\n");
        let log_output = String::from_utf8(
            captured_logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;

        assert!(
            caller_output.contains("UpstreamInvalidResponse"),
            "existing decoder error classification was not preserved: {caller_output}"
        );
        for output in [&caller_output, &recorder_output, &log_output] {
            assert!(
                !output.contains(NATIVE_SENTINEL),
                "native id leaked: {output}"
            );
            assert!(
                !output.contains(CREDENTIAL_SENTINEL),
                "credential leaked: {output}"
            );
        }
        assert!(caller_output.contains(&public_id));
        assert!(recorder_output.contains(&public_id));
        Ok(())
    }

    #[tokio::test]
    async fn responses_mismatch_native_ids_never_reach_error_recorder_or_logs() -> anyhow::Result<()>
    {
        const CREATED_SENTINEL: &str = "native-created-private-sentinel";
        const TERMINAL_SENTINEL: &str = "native-terminal-private-sentinel";
        let upstream = MockServer::start().await;
        let body = format!(
            "event: response.created\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            serde_json::json!({
                "type": "response.created",
                "response": {"id": CREATED_SENTINEL, "status": "in_progress"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": TERMINAL_SENTINEL,
                    "status": "completed",
                    "output": []
                }
            })
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&upstream)
            .await;

        let registry = registry(62).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let mut upstream_target = target("credential");
        upstream_target.api_base = upstream.uri();
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![upstream_target]);
        let recorder_snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(Arc::new(HttpExecutor::with_defaults()?))
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(NativeIdStealingRecorder(recorder_snapshots.clone()));
        let pipeline = Arc::new(builder.build()?);

        let captured_logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_sink = captured_logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || CapturedLogWriter(log_sink.clone()))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let request_id = "responses-mismatch-private-errors";
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request(request_id, None))
            .await?;
        let mut caller_errors = Vec::new();
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                caller_errors.push(error.to_string());
            }
        }
        pipeline.drain_pending_settlements().await;

        let recorder_output = recorder_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .join("\n");
        let log_output = String::from_utf8(
            captured_logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        let caller_output = caller_errors.join("\n");
        assert!(
            !caller_output.is_empty(),
            "the mismatched terminal must fail"
        );
        for output in [&caller_output, &recorder_output, &log_output] {
            assert!(
                !output.contains(CREATED_SENTINEL),
                "created id leaked: {output}"
            );
            assert!(
                !output.contains(TERMINAL_SENTINEL),
                "terminal id leaked: {output}"
            );
        }
        assert_eq!(
            registry
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id(request_id)?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "a mismatched terminal must not publish a continuation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stream_hook_removed_or_failed_downstream_terminal_never_publishes()
    -> anyhow::Result<()> {
        for (index, mutation) in [
            TerminalMutation::Drop,
            TerminalMutation::ReplaceFailed,
            TerminalMutation::ReplaceNonterminal,
        ]
        .into_iter()
        .enumerate()
        {
            let registry = registry(50 + index as u8).await?;
            let runtime = ContinuationRuntime::new(registry.clone());
            let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
            let routes = Arc::new(StaticRoutingTable::new());
            routes.insert("gpt-5", vec![target("credential")]);
            let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
                StreamPart::ResponseStarted {
                    id: "provider-hook-terminal".into(),
                    source_protocol: ApiProtocol::Responses,
                },
                StreamPart::ResponseCompleted {
                    id: "provider-hook-terminal".into(),
                    source_protocol: ApiProtocol::Responses,
                    status: "completed".into(),
                    usage: None,
                    response_output_commitment: None,
                },
            ])]));
            let mut builder = PipelineBuilder::new();
            builder
                .routing_table(routes)
                .executor(executor)
                .route_hook(runtime.clone())
                .stream_hook(TerminalMutatingHook(mutation))
                .required_finalizer(runtime)
                .observe_hook(OutcomeObserver(outcomes.clone()));
            let pipeline = Arc::new(builder.build()?);
            let request_id = format!("hook-terminal-{index}");
            let mut stream = pipeline
                .clone()
                .execute_stream(tool_request(&request_id, None))
                .await?;
            let mut saw_error = false;
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    saw_error = true;
                }
            }
            pipeline.drain_pending_settlements().await;

            assert_eq!(
                registry
                    .resolve(
                        "tool-owner",
                        &encode_gateway_continuation_id(&request_id)?,
                        Utc::now(),
                    )
                    .await?,
                ContinuationResolution::Missing,
                "{mutation:?} exposed a resumable continuation"
            );
            if !matches!(mutation, TerminalMutation::ReplaceFailed) {
                assert!(
                    saw_error,
                    "{mutation:?} must surface the missing downstream success terminal"
                );
            }
            assert_eq!(
                outcomes
                    .lock()
                    .expect("outcome observer poisoned")
                    .as_slice(),
                &["failed"],
                "settlement followed the upstream terminal instead of {mutation:?} downstream output"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_finalizer_with_late_db_completion_is_compensated_after_completion()
    -> anyhow::Result<()> {
        let registry = registry(55).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(tokio::sync::Notify::new());
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            GenerateResult {
                content: vec![Content::Text {
                    text: "ready".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: Some("provider-late-db".into()),
                stop_details: None,
                provider_metadata: Default::default(),
            },
        )]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone())
            .required_finalizer(LateCompletingContinuationFinalizer {
                runtime: runtime.clone(),
                started: started.clone(),
                release: release.clone(),
                completed: completed.clone(),
            });
        let pipeline = Arc::new(builder.build()?);
        let execution = tokio::spawn({
            let pipeline = pipeline.clone();
            async move {
                pipeline
                    .execute(nonstream_tool_request("late-db-cancel", None))
                    .await
            }
        });
        started.notified().await;
        execution.abort();
        let _ = execution.await;

        // The finalizer DB await is shutdown-tracked. Cancellation drops its
        // result receiver; once the late operation completes, that same worker
        // performs compensation before graceful drain returns.
        release.notify_one();
        completed.notified().await;
        pipeline.drain_pending_settlements().await;

        let fresh = ContinuationRegistry::new(
            registry.inner.db.clone(),
            registry.inner.keys.clone(),
            30,
            10,
        )?;
        assert_eq!(
            fresh
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id("late-db-cancel")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "late DB completion escaped the cancellation compensation task"
        );
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    fn terminal_barrier_pipeline(
        registry: ContinuationRegistry,
        blocker: Option<BlockingRequiredFinalizer>,
    ) -> anyhow::Result<Arc<Pipeline>> {
        let runtime = ContinuationRuntime::new(registry);
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ResponseStarted {
                id: "provider-final".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::TextDelta {
                text: "ready".into(),
            },
            StreamPart::ResponseCompleted {
                id: "provider-final".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            },
        ])]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone());
        if let Some(blocker) = blocker {
            builder.required_finalizer(ComposedContinuationFinalizer {
                runtime,
                blocker,
                continuation_first: false,
            });
        } else {
            builder.required_finalizer(runtime);
        }
        Ok(Arc::new(builder.build()?))
    }

    fn public_sse_pipeline(
        registry: ContinuationRegistry,
        outcomes: Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> anyhow::Result<Arc<Pipeline>> {
        let runtime = ContinuationRuntime::new(registry);
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ResponseStarted {
                id: "provider-public-sse".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::TextDelta {
                text: "ready".into(),
            },
            StreamPart::ResponseCompleted {
                id: "provider-public-sse".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            },
        ])]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .observe_hook(OutcomeObserver(outcomes));
        Ok(Arc::new(builder.build()?))
    }

    #[tokio::test]
    async fn public_sse_drop_after_early_terminal_expansion_frame_never_publishes()
    -> anyhow::Result<()> {
        let registry = registry(53).await?;
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pipeline = public_sse_pipeline(registry.clone(), outcomes.clone())?;
        let response = build_router(app_state(pipeline.clone()))
            .oneshot(responses_http_request("public-sse-early-drop", true))
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let mut saw_early_terminal_frame = false;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            let wire = String::from_utf8_lossy(&chunk);
            if wire.contains("event: response.output_text.done") {
                saw_early_terminal_frame = true;
                break;
            }
        }
        assert!(
            saw_early_terminal_frame,
            "Responses terminal expansion did not expose its pre-terminal done frame"
        );
        drop(body);
        pipeline.drain_pending_settlements().await;

        assert_eq!(
            registry
                .resolve(
                    CallerContext::local().user_id(),
                    &encode_gateway_continuation_id("public-sse-early-drop")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing,
            "an earlier frame from a multi-frame terminal expansion authorized continuation"
        );
        assert_eq!(
            outcomes
                .lock()
                .expect("outcome observer poisoned")
                .as_slice(),
            &["disconnected"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn public_sse_final_success_frame_immediately_authorizes_continuation()
    -> anyhow::Result<()> {
        let registry = registry(54).await?;
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pipeline = public_sse_pipeline(registry.clone(), outcomes)?;
        let response = build_router(app_state(pipeline.clone()))
            .oneshot(responses_http_request("public-sse-final-frame", true))
            .await?;
        let mut body = response.into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            if String::from_utf8_lossy(&chunk).contains("event: response.completed") {
                assert!(matches!(
                    registry
                        .resolve(
                            CallerContext::local().user_id(),
                            &encode_gateway_continuation_id("public-sse-final-frame")?,
                            Utc::now(),
                        )
                        .await?,
                    ContinuationResolution::Active(_)
                ));
                drop(body);
                pipeline.drain_pending_settlements().await;
                return Ok(());
            }
        }
        anyhow::bail!("public response.completed frame missing")
    }

    #[tokio::test]
    async fn disconnect_while_terminal_finalizer_is_blocked_never_binds() -> anyhow::Result<()> {
        let registry = registry(31).await?;
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let pipeline = terminal_barrier_pipeline(
            registry.clone(),
            Some(BlockingRequiredFinalizer {
                started: started.clone(),
                release: release.clone(),
                completed: completed.clone(),
            }),
        )?;
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request("blocked-terminal", None))
            .await?;
        assert!(matches!(
            stream.next().await.transpose()?,
            Some(StreamPart::ResponseStarted { .. })
        ));
        assert!(matches!(
            stream.next().await.transpose()?,
            Some(StreamPart::TextDelta { .. })
        ));

        let poller = tokio::spawn(async move { stream.next().await });
        started.notified().await;
        poller.abort();
        let _ = poller.await;
        release.notify_one();
        pipeline.drain_pending_settlements().await;

        assert!(
            completed.load(Ordering::SeqCst),
            "tracked finalizer preparation completes before compensating cancellation"
        );
        let public_id = encode_gateway_continuation_id("blocked-terminal")?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "a dropped terminal poll must not transiently or durably publish"
        );
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_after_continuation_side_effect_rolls_back_before_publication()
    -> anyhow::Result<()> {
        let registry = registry(38).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ResponseStarted {
                id: "provider-post-side-effect".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::ResponseCompleted {
                id: "provider-post-side-effect".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: None,
                response_output_commitment: None,
            },
        ])]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone())
            // The continuation DB write deliberately happens first inside one
            // atomic success-critical composition.
            .required_finalizer(ComposedContinuationFinalizer {
                runtime,
                blocker: BlockingRequiredFinalizer {
                    started: started.clone(),
                    release: release.clone(),
                    completed: completed.clone(),
                },
                continuation_first: true,
            });
        let pipeline = Arc::new(builder.build()?);
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request("post-side-effect", None))
            .await?;
        assert!(stream.next().await.transpose()?.is_some());
        let poller = tokio::spawn(async move { stream.next().await });
        started.notified().await;

        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 1);
        let public_id = encode_gateway_continuation_id("post-side-effect")?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "provisional DB state must remain unpublished"
        );

        poller.abort();
        let _ = poller.await;
        release.notify_one();
        pipeline.drain_pending_settlements().await;
        assert!(
            completed.load(Ordering::SeqCst),
            "tracked finalizer preparation completes before compensating cancellation"
        );
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing
        );
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn nonstream_cancellation_during_settlement_rolls_back_provisional_mapping()
    -> anyhow::Result<()> {
        let registry = registry(42).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Generate(
            GenerateResult {
                content: vec![Content::Text {
                    text: "ready".into(),
                    provider_metadata: Default::default(),
                }],
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: Some("provider-nonstream-final".into()),
                stop_details: None,
                provider_metadata: Default::default(),
            },
        )]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone())
            .required_finalizer(runtime)
            .settlement_recorder(BlockingSettlementRecorder {
                started: started.clone(),
                release: release.clone(),
            });
        let pipeline = Arc::new(builder.build()?);
        let execution = {
            let pipeline = pipeline.clone();
            tokio::spawn(async move {
                pipeline
                    .execute(nonstream_tool_request("nonstream-cancel-tail", None))
                    .await
            })
        };
        started.notified().await;

        let public_id = encode_gateway_continuation_id("nonstream-cancel-tail")?;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing,
            "prepared state must remain hidden while settlement is pending"
        );
        execution.abort();
        let _ = execution.await;
        release.notify_one();
        pipeline.drain_pending_settlements().await;
        assert_eq!(
            registry
                .resolve("tool-owner", &public_id, Utc::now())
                .await?,
            ContinuationResolution::Missing
        );
        let row = registry
            .database()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("continuation count missing"))?;
        assert_eq!(row.try_get::<i64>("", "count")?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_while_on_stream_end_is_blocked_completes_hooks_and_settles()
    -> anyhow::Result<()> {
        let registry = registry(37).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let usage = Arc::new(std::sync::Mutex::new(Vec::new()));
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let routes = Arc::new(StaticRoutingTable::new());
        routes.insert("gpt-5", vec![target("credential")]);
        let executor = Arc::new(MockExecutor::new(vec![MockResponse::Stream(vec![
            StreamPart::ResponseStarted {
                id: "provider-hook-final".into(),
                source_protocol: ApiProtocol::Responses,
            },
            StreamPart::TextDelta {
                text: "ready".into(),
            },
            StreamPart::ResponseCompleted {
                id: "provider-hook-final".into(),
                source_protocol: ApiProtocol::Responses,
                status: "completed".into(),
                usage: Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    ..Default::default()
                }),
                response_output_commitment: None,
            },
        ])]));
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(routes)
            .executor(executor)
            .route_hook(runtime.clone())
            .stream_hook(BlockingEndHook {
                started: started.clone(),
                release: release.clone(),
                completed: completed.clone(),
            })
            .required_finalizer(runtime)
            .settlement_recorder(UsageSettlementRecorder(usage.clone()))
            .observe_hook(OutcomeObserver(outcomes.clone()));
        let pipeline = Arc::new(builder.build()?);
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request("blocked-end-hook", None))
            .await?;
        assert!(stream.next().await.transpose()?.is_some());
        assert!(stream.next().await.transpose()?.is_some());
        let poller = tokio::spawn(async move { stream.next().await });
        started.notified().await;
        poller.abort();
        let _ = poller.await;
        release.notify_one();
        pipeline.drain_pending_settlements().await;

        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            usage
                .lock()
                .expect("usage settlement state poisoned")
                .as_slice(),
            &[(7, 3)]
        );
        assert_eq!(
            outcomes
                .lock()
                .expect("outcome observer poisoned")
                .as_slice(),
            &["disconnected"]
        );
        assert_eq!(
            registry
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id("blocked-end-hook")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_before_terminal_never_binds() -> anyhow::Result<()> {
        let registry = registry(32).await?;
        let pipeline = terminal_barrier_pipeline(registry.clone(), None)?;
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request("before-terminal", None))
            .await?;
        assert!(stream.next().await.transpose()?.is_some());
        drop(stream);
        pipeline.drain_pending_settlements().await;
        assert_eq!(
            registry
                .resolve(
                    "tool-owner",
                    &encode_gateway_continuation_id("before-terminal")?,
                    Utc::now(),
                )
                .await?,
            ContinuationResolution::Missing
        );
        Ok(())
    }

    #[tokio::test]
    async fn first_returned_success_terminal_has_immediately_resolvable_public_id()
    -> anyhow::Result<()> {
        let registry = registry(33).await?;
        let pipeline = terminal_barrier_pipeline(registry.clone(), None)?;
        let mut stream = pipeline
            .clone()
            .execute_stream(tool_request("delivered-terminal", None))
            .await?;
        while let Some(part) = stream.next().await {
            if matches!(part?, StreamPart::ResponseCompleted { .. }) {
                let public_id = encode_gateway_continuation_id("delivered-terminal")?;
                assert!(matches!(
                    registry
                        .resolve("tool-owner", &public_id, Utc::now())
                        .await?,
                    ContinuationResolution::Active(_)
                ));
                return Ok(());
            }
        }
        anyhow::bail!("success terminal missing")
    }

    #[tokio::test]
    async fn registry_db_failure_precedes_stream_success_terminal() -> anyhow::Result<()> {
        let registry = registry(17).await?;
        registry
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TABLE provider_continuations".to_owned(),
            ))
            .await?;
        assert_finalizer_failure_precedes_terminal(registry, "pruning provider continuations").await
    }

    #[tokio::test]
    async fn registry_key_epoch_failure_precedes_stream_success_terminal() -> anyhow::Result<()> {
        let original = registry(18).await?;
        original
            .bind(
                "tool-owner",
                "bootstrap",
                "provider-bootstrap",
                &target("credential"),
                &static_authority("credential"),
                Utc::now(),
            )
            .await?;
        let rotated = ContinuationRegistry::new(
            original.database().clone(),
            ContinuationKeySource::fixed(ContinuationKey::from_bytes([19; 32])?),
            30,
            10,
        )?;
        assert_finalizer_failure_precedes_terminal(rotated, "key epoch").await
    }
}
