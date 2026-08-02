//! Encrypted provider continuation registry and pipeline integration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use bitrouter_sdk::error::{BitrouterError, Result as PipelineResult};
use bitrouter_sdk::language_model::context::{
    PipelineContext, ProviderContinuation, RequireContinuationAuthority,
};
use bitrouter_sdk::language_model::hooks::RouteHook;
use bitrouter_sdk::language_model::protocol::responses::{
    decode_gateway_continuation_id, encode_gateway_continuation_id,
};
use bitrouter_sdk::language_model::settlement::{
    RequiredDeliveryHandshake, RequiredFinalizationContext, RequiredFinalizer,
};
use bitrouter_sdk::language_model::{
    ApiProtocol, AuthAppliers, ContinuationAuthority, RoutingTarget,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, DatabaseConnection, QueryOrder, QuerySelect};
use sha2::Sha256;

const OWNER_IDENTITY_DOMAIN: &[u8] = b"bitrouter.continuation.owner.v1";
const CONTINUATION_IDENTITY_DOMAIN: &[u8] = b"bitrouter.continuation.identity.v1";
const TARGET_FINGERPRINT_DOMAIN: &[u8] = b"bitrouter.continuation.target.v1";
const KEY_ID_DOMAIN: &[u8] = b"bitrouter.continuation.key-id.v1";
const AEAD_AAD_DOMAIN: &[u8] = b"bitrouter.continuation.aead.v1";
const CIPHER_VERSION: i32 = 1;
const NONCE_BYTES: usize = 12;
const PUBLICATION_GENERATION_BYTES: usize = 16;
const PUBLICATION_PROVISIONAL: &str = "provisional";
const PUBLICATION_DELIVERING: &str = "delivering";
const PUBLICATION_ACTIVE: &str = "active";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContinuation {
    pub provider_response_id: String,
    target_fingerprint: String,
    key: ContinuationKey,
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
    db: DatabaseConnection,
    keys: ContinuationKeySource,
    retention: TimeDelta,
    prune_batch_size: u64,
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
    #[cfg(test)]
    ambiguous_insert_fault: Arc<Mutex<Option<Arc<AmbiguousInsertFault>>>>,
    #[cfg(test)]
    maintenance_fault: Arc<Mutex<Option<Arc<MaintenanceFault>>>>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOutcome {
    Inserted,
    ExistingActive,
    ExistingProvisional,
}

struct PendingBindAttempt {
    now: DateTime<Utc>,
    delivery_attempt_id: u64,
}

struct BindPublication {
    now: DateTime<Utc>,
    state: &'static str,
    generation: String,
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
            db,
            keys,
            retention,
            prune_batch_size,
            pending_publications: Arc::new(Mutex::new(HashMap::new())),
            pending_bind_locks: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            ambiguous_insert_fault: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            maintenance_fault: Arc::new(Mutex::new(None)),
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
        match self.ambiguous_insert_fault.lock() {
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
        match self.maintenance_fault.lock() {
            Ok(mut installed) => *installed = Some(fault),
            Err(poisoned) => *poisoned.into_inner() = Some(fault),
        }
        (snapshot_read_rx, release_tx)
    }

    #[cfg(test)]
    async fn wait_at_maintenance_snapshot(&self, kind: MaintenanceFaultKind) {
        let fault = {
            let mut installed = match self.maintenance_fault.lock() {
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
        &self.db
    }

    fn pending_identity_lock(&self, continuation_identity: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.pending_bind_locks.lock() {
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

    async fn rollback_pending(&self, delivery_attempt_id: u64) -> Result<()> {
        let publication = {
            let publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            publications.get(&delivery_attempt_id).cloned()
        };
        let Some(publication) = publication else {
            return Ok(());
        };
        // Serialize only the same owner-bound continuation identity. Unrelated
        // callers and request ids retain full DB concurrency.
        let identity_lock = self.pending_identity_lock(&publication.continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        self.compensate_owned_publication(&publication).await?;
        self.clear_pending_publication(delivery_attempt_id, &publication.generation);
        Ok(())
    }

    fn clear_pending_publication(&self, delivery_attempt_id: u64, generation: &str) {
        let mut publications = match self.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        if publications
            .get(&delivery_attempt_id)
            .is_some_and(|publication| publication.generation == generation)
        {
            publications.remove(&delivery_attempt_id);
        }
    }

    async fn compensate_owned_publication(&self, publication: &PendingPublication) -> Result<()> {
        let Some(mut row) =
            continuation_entity::Entity::find_by_id(&publication.continuation_identity)
                .one(&self.db)
                .await?
        else {
            return Ok(());
        };
        if row.publication_generation != publication.generation {
            anyhow::bail!("continuation rollback no longer owns the durable generation");
        }
        let key = self.keys.load()?;
        match row.publication_state.as_str() {
            PUBLICATION_DELIVERING | PUBLICATION_ACTIVE => {
                row = self
                    .transition_publication_state(&key, &row, PUBLICATION_PROVISIONAL)
                    .await?;
            }
            PUBLICATION_PROVISIONAL => {
                decrypt_row(&key, &row)?;
            }
            state => anyhow::bail!("unsupported continuation publication state '{state}'"),
        }
        let ciphertext = row
            .ciphertext
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
        let nonce = row
            .nonce
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
        let deleted = continuation_entity::Entity::delete_many()
            .filter(
                continuation_entity::Column::ContinuationIdentity
                    .eq(row.continuation_identity.clone()),
            )
            .filter(continuation_entity::Column::PublicationState.eq(PUBLICATION_PROVISIONAL))
            .filter(
                continuation_entity::Column::PublicationGeneration
                    .eq(publication.generation.clone()),
            )
            .filter(continuation_entity::Column::Ciphertext.eq(ciphertext.clone()))
            .filter(continuation_entity::Column::Nonce.eq(nonce.clone()))
            .exec(&self.db)
            .await?;
        if deleted.rows_affected == 1 {
            return Ok(());
        }
        if continuation_entity::Entity::find_by_id(&publication.continuation_identity)
            .one(&self.db)
            .await?
            .is_none()
        {
            return Ok(());
        }
        anyhow::bail!("continuation rollback compare-and-swap lost ownership")
    }

    async fn activate_pending(
        &self,
        delivery_attempt_id: u64,
        delivery: &RequiredDeliveryHandshake,
    ) -> Result<bool> {
        let publication = {
            let publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            publications.get(&delivery_attempt_id).cloned()
        };
        let Some(publication) = publication else {
            // Idempotent preparation against an already-active row owns no
            // durable transition but must still use the delivery rendezvous.
            return delivery
                .wait_for_delivery()
                .await
                .map_err(anyhow::Error::from);
        };
        let identity_lock = self.pending_identity_lock(&publication.continuation_identity);
        let _identity_guard = identity_lock.lock().await;
        let key = self.keys.load()?;
        let row = continuation_entity::Entity::find_by_id(&publication.continuation_identity)
            .one(&self.db)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("provisional continuation disappeared before activation")
            })?;
        if row.publication_generation != publication.generation {
            anyhow::bail!("provisional continuation generation ownership changed");
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

        let delivery_result = delivery.wait_for_delivery_acknowledgement().await;
        if delivery_result.as_ref().is_ok_and(|delivered| *delivered) {
            let row = continuation_entity::Entity::find_by_id(&publication.continuation_identity)
                .one(&self.db)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("delivering continuation disappeared before activation")
                })?;
            let activation = if row.publication_generation != publication.generation {
                Err(anyhow::anyhow!(
                    "delivering continuation generation ownership changed"
                ))
            } else if row.publication_state != PUBLICATION_DELIVERING {
                Err(anyhow::anyhow!(
                    "continuation left delivering state before activation"
                ))
            } else {
                self.transition_publication_state(&key, &row, PUBLICATION_ACTIVE)
                    .await
            };
            match activation {
                Ok(_) => {
                    let activation_observed = delivery.complete_activation(Ok(()));
                    if activation_observed && delivery.wait_for_terminal_commit().await {
                        self.clear_pending_publication(
                            delivery_attempt_id,
                            &publication.generation,
                        );
                        return Ok(true);
                    }
                    match self.compensate_owned_publication(&publication).await {
                        Ok(()) => {
                            self.clear_pending_publication(
                                delivery_attempt_id,
                                &publication.generation,
                            );
                            return Ok(false);
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(activation_error) => {
                    let compensation = self.compensate_owned_publication(&publication).await;
                    if compensation.is_ok() {
                        self.clear_pending_publication(
                            delivery_attempt_id,
                            &publication.generation,
                        );
                    }
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

        match self.compensate_owned_publication(&publication).await {
            Ok(()) => {
                self.clear_pending_publication(delivery_attempt_id, &publication.generation);
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
        let old_ciphertext = row
            .ciphertext
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
        let old_nonce = row
            .nonce
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("continuation has expired"))?;
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
        ]);
        let (ciphertext, nonce) = encrypt(key, plaintext.as_bytes(), &aad)?;
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
            .filter(
                continuation_entity::Column::ContinuationIdentity
                    .eq(row.continuation_identity.clone()),
            )
            .filter(continuation_entity::Column::PublicationState.eq(row.publication_state.clone()))
            .filter(
                continuation_entity::Column::PublicationGeneration
                    .eq(row.publication_generation.clone()),
            )
            .filter(continuation_entity::Column::Ciphertext.eq(old_ciphertext.clone()))
            .filter(continuation_entity::Column::Nonce.eq(old_nonce.clone()))
            .exec(&self.db)
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
        let key = self.keys.load()?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        let outcome = self
            .bind_inner(
                owner_user_id,
                gateway_request_id,
                provider_response_id,
                target,
                credential_authority,
                BindPublication {
                    now,
                    state: PUBLICATION_ACTIVE,
                    generation: publication_generation()?,
                },
            )
            .await?;
        if outcome == BindOutcome::ExistingProvisional {
            anyhow::bail!("gateway continuation id has a pending publication");
        }
        Ok(())
    }

    async fn bind_pending(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        provider_response_id: &str,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
        attempt: PendingBindAttempt,
    ) -> Result<bool> {
        let PendingBindAttempt {
            now,
            delivery_attempt_id,
        } = attempt;
        let key = self.keys.load()?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let generation = publication_generation()?;
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        {
            let mut publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            match publications.entry(delivery_attempt_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(PendingPublication {
                        continuation_identity: continuation_identity.clone(),
                        generation: generation.clone(),
                    });
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    anyhow::bail!("duplicate continuation delivery attempt id");
                }
            }
        }
        let result = self
            .bind_inner(
                owner_user_id,
                gateway_request_id,
                provider_response_id,
                target,
                credential_authority,
                BindPublication {
                    now,
                    state: PUBLICATION_PROVISIONAL,
                    generation: generation.clone(),
                },
            )
            .await;
        match result {
            Ok(BindOutcome::Inserted) => Ok(true),
            Ok(BindOutcome::ExistingActive) => {
                self.clear_pending_publication(delivery_attempt_id, &generation);
                Ok(false)
            }
            Ok(BindOutcome::ExistingProvisional) => {
                self.clear_pending_publication(delivery_attempt_id, &generation);
                anyhow::bail!("gateway continuation id has a pending publication")
            }
            // Retain ownership across ambiguous insert errors. The caller's
            // tracked rollback confirms whether this generation committed.
            Err(error) => Err(error),
        }
    }

    async fn bind_inner(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        provider_response_id: &str,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
        publication: BindPublication,
    ) -> Result<BindOutcome> {
        let BindPublication {
            now,
            state,
            generation,
        } = publication;
        let key = self.keys.load()?;
        self.ensure_key_epoch(&key, now).await?;
        let owner_identity = key.owner_identity(owner_user_id)?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let target_fingerprint = key.target_fingerprint(target, credential_authority)?;
        let created_at = timestamp(now);
        let expires_at = timestamp(
            now.checked_add_signed(self.retention)
                .ok_or_else(|| anyhow::anyhow!("continuation expiry exceeds time range"))?,
        );
        let purge_after = timestamp(
            now.checked_add_signed(self.retention + self.retention)
                .ok_or_else(|| anyhow::anyhow!("continuation purge boundary exceeds time range"))?,
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
        ]);
        let (ciphertext, nonce) = encrypt(&key, provider_response_id.as_bytes(), &aad)?;
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
        };
        let insert_result = model.insert(&self.db).await;
        #[cfg(test)]
        let insert_result = match insert_result {
            Ok(model) => {
                let fault = match self.ambiguous_insert_fault.lock() {
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
                    .one(&self.db)
                    .await?;
                let Some(existing) = existing else {
                    return Err(insert_error.into());
                };
                let existing_plaintext = decrypt_row(&key, &existing)?;
                if existing_plaintext == provider_response_id
                    && existing.target_fingerprint == target_fingerprint
                {
                    if existing.publication_generation == generation {
                        return match existing.publication_state.as_str() {
                            PUBLICATION_PROVISIONAL
                            | PUBLICATION_DELIVERING
                            | PUBLICATION_ACTIVE => Ok(BindOutcome::Inserted),
                            state => anyhow::bail!(
                                "unsupported continuation publication state '{state}'"
                            ),
                        };
                    }
                    match existing.publication_state.as_str() {
                        PUBLICATION_ACTIVE => Ok(BindOutcome::ExistingActive),
                        PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING => {
                            Ok(BindOutcome::ExistingProvisional)
                        }
                        state => {
                            anyhow::bail!("unsupported continuation publication state '{state}'")
                        }
                    }
                } else {
                    anyhow::bail!("gateway continuation id is already bound to another response")
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
        let key = self.keys.load()?;
        self.ensure_key_epoch(&key, now).await?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        // The same-identity lock linearizes resolve with bind, activation, and
        // rollback, including the old precheck-to-insert TOCTOU window.
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _identity_guard = identity_lock.lock().await;
        loop {
            let Some(row) = continuation_entity::Entity::find_by_id(&continuation_identity)
                .one(&self.db)
                .await?
            else {
                return Ok(ContinuationResolution::Missing);
            };
            if row.publication_state != PUBLICATION_ACTIVE {
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
            let provider_response_id = decrypt_row(&key, &row)?;
            return Ok(ContinuationResolution::Active(ResolvedContinuation {
                provider_response_id,
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
            .limit(self.prune_batch_size)
            .all(&self.db)
            .await?;
        for row in expired {
            self.scrub_expired(&row).await?;
        }

        let purge_rows = continuation_entity::Entity::find()
            .filter(continuation_entity::Column::PurgeAfter.lte(now))
            .order_by_asc(continuation_entity::Column::PurgeAfter)
            .limit(self.prune_batch_size)
            .all(&self.db)
            .await?;
        #[cfg(test)]
        self.wait_at_maintenance_snapshot(MaintenanceFaultKind::Purge)
            .await;
        let mut rows_affected = 0;
        for row in purge_rows {
            let result = continuation_entity::Entity::delete_many()
                .filter(continuation_snapshot_condition(&row))
                .exec(&self.db)
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
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected == 1)
    }

    async fn ensure_key_epoch(&self, key: &ContinuationKey, now: DateTime<Utc>) -> Result<()> {
        let existing = key_epoch_entity::Entity::find_by_id(1)
            .one(&self.db)
            .await?;
        let epoch = match existing {
            Some(epoch) => epoch,
            None => {
                let insert = key_epoch_entity::ActiveModel {
                    singleton_id: Set(1),
                    key_id: Set(key.key_id.clone()),
                    created_at: Set(timestamp(now)),
                };
                match insert.insert(&self.db).await {
                    Ok(epoch) => epoch,
                    Err(_) => key_epoch_entity::Entity::find_by_id(1)
                        .one(&self.db)
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

/// Always-on route resolver and success-critical continuation publisher.
#[derive(Clone)]
pub struct ContinuationRuntime {
    registry: ContinuationRegistry,
    auth_appliers: AuthAppliers,
}

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
        let resolution = self
            .registry
            .resolve(ctx.caller().user_id(), &previous_response_id, Utc::now())
            .await
            .map_err(|error| {
                BitrouterError::internal(format!(
                    "resolving provider continuation failed closed: {error}"
                ))
            })?;
        match resolution {
            ContinuationResolution::Active(active) => {
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

#[async_trait]
impl RequiredFinalizer for ContinuationRuntime {
    async fn finalize(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
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
        self.registry.prune(now).await.map_err(|error| {
            BitrouterError::internal(format!("pruning provider continuations: {error}"))
        })?;
        self.registry
            .bind_pending(
                ctx.caller.user_id(),
                &public_continuation_id,
                provider_response_id,
                target,
                &credential_authority,
                PendingBindAttempt {
                    now,
                    delivery_attempt_id: ctx.delivery_attempt_id,
                },
            )
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("persisting provider continuation: {error}"))
            })?;
        Ok(())
    }

    async fn rollback(&self, ctx: &RequiredFinalizationContext) -> PipelineResult<()> {
        if ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return Ok(());
        }
        self.registry
            .rollback_pending(ctx.delivery_attempt_id)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!(
                    "rolling back provider continuation publication: {error}"
                ))
            })
    }

    async fn commit(
        &self,
        ctx: &RequiredFinalizationContext,
        delivery: &RequiredDeliveryHandshake,
    ) -> PipelineResult<bool> {
        if ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return delivery.wait_for_delivery().await;
        }
        match self
            .registry
            .activate_pending(ctx.delivery_attempt_id, delivery)
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

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("invalid continuation timestamp '{value}'"))
}

fn aead_aad(fields: [&str; 9]) -> Vec<u8> {
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

fn decrypt_row(key: &ContinuationKey, row: &continuation_entity::Model) -> Result<String> {
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
    String::from_utf8(plaintext.to_vec()).context("continuation plaintext is not valid UTF-8")
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
    use bitrouter_sdk::language_model::context::{PipelineContext, ProviderContinuation};
    use bitrouter_sdk::language_model::hooks::{
        ObserveHook, RequestOutcome, RouteHook, StreamHook,
    };
    use bitrouter_sdk::language_model::settlement::{
        DeliveryAcknowledgement, RequiredFinalizationContext, RequiredFinalizer, SettlementContext,
        SettlementRecorder,
    };
    use bitrouter_sdk::language_model::{
        ApiProtocol, AppliedAuth, AuthApplier, AuthAppliers, Content, CredentialAuthority,
        ExecutionResult, Executor, FinishReason, GenerateResult, GenerationParams, HttpExecutor,
        Message, MockExecutor, MockResponse, Pipeline, PipelineBuilder, PipelineRequest, Prompt,
        Role, RoutingTarget, StaticRoutingTable, StreamAction, StreamContext, StreamInterest,
        StreamOutcome, StreamPart, StreamPartStream, Tool, ToolResultOutput, Usage,
    };
    use bitrouter_sdk::server::{AppState, build_router};
    use chrono::{TimeDelta, TimeZone, Utc};
    use futures::StreamExt;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

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
        let now = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).single().unwrap();
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

        let independent =
            ContinuationRegistry::new(registry.db.clone(), registry.keys.clone(), 30, 10)?;
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

    fn continuation_context(previous_response_id: &str) -> PipelineContext {
        let mut params = GenerationParams::default();
        params.extra.insert(
            "previous_response_id".into(),
            serde_json::Value::String(previous_response_id.into()),
        );
        PipelineContext::new(PipelineRequest {
            request_id: "next-gateway".into(),
            model: "gpt-5".into(),
            caller: CallerContext::new("key", "owner"),
            headers: Default::default(),
            prompt: Prompt {
                model: "gpt-5".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![Message::text(Role::User, "continue")],
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
    async fn required_finalizer_publishes_native_mapping() -> anyhow::Result<()> {
        let registry = registry(14).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let finalization = finalization_context("gateway-result", 1, "resp-provider-final");
        runtime.finalize(&finalization).await?;
        commit_delivered(&runtime, &finalization).await?;
        let resolved = registry
            .resolve(
                "owner",
                &encode_gateway_continuation_id("gateway-result")?,
                Utc::now(),
            )
            .await?;
        assert!(matches!(resolved, ContinuationResolution::Active(_)));
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
        let restarted =
            ContinuationRegistry::new(registry.db.clone(), registry.keys.clone(), 30, 10)?;
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

        let fresh = ContinuationRegistry::new(registry.db.clone(), registry.keys.clone(), 30, 10)?;
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

        let independent =
            ContinuationRegistry::new(registry.db.clone(), registry.keys.clone(), 30, 10)?;
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
            first_registry.db.clone(),
            first_registry.keys.clone(),
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
        let continuation_identity = first_registry.keys.load()?.continuation_identity(
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

        let fresh = ContinuationRegistry::new(registry.db.clone(), registry.keys.clone(), 30, 10)?;
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
