//! Encrypted provider continuation registry and pipeline integration.

use std::collections::{HashMap, HashSet};
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
use bitrouter_sdk::language_model::settlement::{RequiredFinalizationContext, RequiredFinalizer};
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
    /// Process-local delivery barrier. Normal cancellation and graceful
    /// shutdown are compensated by tracked rollback before this mask is
    /// removed. A hard process crash after the encrypted row insert is an
    /// intentionally documented at-least-once ambiguity: the provider already
    /// completed, so the orphaned mapping may be visible after restart.
    pending_publications: Arc<Mutex<HashMap<String, PendingPublication>>>,
    pending_bind_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
}

#[derive(Default)]
struct PendingPublication {
    attempts: HashSet<u64>,
    row_created: bool,
    published: bool,
}

struct PendingBindAttempt {
    now: DateTime<Utc>,
    delivery_attempt_id: u64,
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
        })
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.db
    }

    fn is_pending_identity(&self, continuation_identity: &str) -> bool {
        match self.pending_publications.lock() {
            Ok(publications) => publications
                .get(continuation_identity)
                .is_some_and(|publication| !publication.published),
            Err(poisoned) => poisoned
                .into_inner()
                .get(continuation_identity)
                .is_some_and(|publication| !publication.published),
        }
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

    fn publish_pending(&self, delivery_attempt_id: u64) {
        let mut publications = match self.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        let identity = publications.iter().find_map(|(identity, publication)| {
            publication
                .attempts
                .contains(&delivery_attempt_id)
                .then(|| identity.clone())
        });
        let Some(identity) = identity else {
            return;
        };
        let remove = publications.get_mut(&identity).is_some_and(|publication| {
            if !publication.attempts.remove(&delivery_attempt_id) {
                return false;
            }
            publication.published = true;
            publication.attempts.is_empty()
        });
        if remove {
            publications.remove(&identity);
        }
    }

    async fn rollback_pending(&self, delivery_attempt_id: u64) -> Result<()> {
        let identity = {
            let publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            publications.iter().find_map(|(identity, publication)| {
                publication
                    .attempts
                    .contains(&delivery_attempt_id)
                    .then(|| identity.clone())
            })
        };
        let Some(identity) = identity else {
            return Ok(());
        };
        // Serialize only the same owner-bound continuation identity. Unrelated
        // callers and request ids retain full DB concurrency.
        let identity_lock = self.pending_identity_lock(&identity);
        let _bind_guard = identity_lock.lock().await;
        let delete = {
            let mut publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(publication) = publications.get_mut(&identity) else {
                return Ok(());
            };
            if !publication.attempts.remove(&delivery_attempt_id) {
                return Ok(());
            }
            if !publication.attempts.is_empty() {
                return Ok(());
            }
            if publication.published || !publication.row_created {
                publications.remove(&identity);
                return Ok(());
            }
            // Keep the unpublished marker installed until the delete commits,
            // so a concurrent resolve cannot observe provisional DB state.
            true
        };
        if delete {
            continuation_entity::Entity::delete_by_id(identity.clone())
                .exec(&self.db)
                .await?;
            let mut publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            publications.remove(&identity);
        }
        Ok(())
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
        self.bind_inner(
            owner_user_id,
            gateway_request_id,
            provider_response_id,
            target,
            credential_authority,
            now,
        )
        .await
        .map(drop)
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
        let identity_lock = self.pending_identity_lock(&continuation_identity);
        let _bind_guard = identity_lock.lock().await;
        let joined_pending = {
            let mut publications = match self.pending_publications.lock() {
                Ok(publications) => publications,
                Err(poisoned) => poisoned.into_inner(),
            };
            let joined = publications.contains_key(&continuation_identity);
            publications
                .entry(continuation_identity.clone())
                .or_default()
                .attempts
                .insert(delivery_attempt_id);
            joined
        };
        let result = self
            .bind_inner(
                owner_user_id,
                gateway_request_id,
                provider_response_id,
                target,
                credential_authority,
                now,
            )
            .await;
        let mut publications = match self.pending_publications.lock() {
            Ok(publications) => publications,
            Err(poisoned) => poisoned.into_inner(),
        };
        match result {
            Ok(inserted) => {
                let publication = publications
                    .get_mut(&continuation_identity)
                    .expect("pending continuation reservation disappeared");
                if inserted {
                    publication.row_created = true;
                } else if !joined_pending {
                    // An idempotent match against a row that predates this
                    // attempt is already public and must not become rollback
                    // owned by this request.
                    publication.attempts.remove(&delivery_attempt_id);
                    if publication.attempts.is_empty() {
                        publications.remove(&continuation_identity);
                    }
                }
                Ok(inserted)
            }
            Err(error) => {
                if let Some(publication) = publications.get_mut(&continuation_identity) {
                    publication.attempts.remove(&delivery_attempt_id);
                    if publication.attempts.is_empty() {
                        publications.remove(&continuation_identity);
                    }
                }
                Err(error)
            }
        }
    }

    async fn bind_inner(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        provider_response_id: &str,
        target: &RoutingTarget,
        credential_authority: &ContinuationAuthority,
        now: DateTime<Utc>,
    ) -> Result<bool> {
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
        let aad = aead_aad(
            &key.key_id,
            &owner_identity,
            &continuation_identity,
            &target_fingerprint,
            &created_at,
            &expires_at,
            &purge_after,
        );
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
        };
        match model.insert(&self.db).await {
            Ok(_) => Ok(true),
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
                    Ok(false)
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
        if self.is_pending_identity(&continuation_identity) {
            return Ok(ContinuationResolution::Missing);
        }
        let Some(row) = continuation_entity::Entity::find_by_id(&continuation_identity)
            .one(&self.db)
            .await?
        else {
            return Ok(ContinuationResolution::Missing);
        };
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
            self.scrub_expired(&row.continuation_identity).await?;
            return Ok(ContinuationResolution::Expired);
        }
        let provider_response_id = decrypt_row(&key, &row)?;
        Ok(ContinuationResolution::Active(ResolvedContinuation {
            provider_response_id,
            target_fingerprint: row.target_fingerprint,
            key,
        }))
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
            self.scrub_expired(&row.continuation_identity).await?;
        }

        let purge_ids = continuation_entity::Entity::find()
            .select_only()
            .column(continuation_entity::Column::ContinuationIdentity)
            .filter(continuation_entity::Column::PurgeAfter.lte(now))
            .order_by_asc(continuation_entity::Column::PurgeAfter)
            .limit(self.prune_batch_size)
            .into_tuple::<String>()
            .all(&self.db)
            .await?;
        if purge_ids.is_empty() {
            return Ok(0);
        }
        let result = continuation_entity::Entity::delete_many()
            .filter(continuation_entity::Column::ContinuationIdentity.is_in(purge_ids))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected)
    }

    async fn scrub_expired(&self, continuation_identity: &str) -> Result<()> {
        let model = continuation_entity::ActiveModel {
            continuation_identity: sea_orm::ActiveValue::Unchanged(
                continuation_identity.to_owned(),
            ),
            ciphertext: Set(None),
            nonce: Set(None),
            ..Default::default()
        };
        model.update(&self.db).await?;
        Ok(())
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

    fn commit(&self, ctx: &RequiredFinalizationContext) {
        if ctx.inbound_protocol != Some(ApiProtocol::Responses) {
            return;
        }
        self.registry.publish_pending(ctx.delivery_attempt_id);
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

fn aead_aad(
    key_id: &str,
    owner_identity: &str,
    continuation_identity: &str,
    target_fingerprint: &str,
    created_at: &str,
    expires_at: &str,
    purge_after: &str,
) -> Vec<u8> {
    let mut aad = AEAD_AAD_DOMAIN.to_vec();
    for field in [
        key_id,
        owner_identity,
        continuation_identity,
        target_fingerprint,
        created_at,
        expires_at,
        purge_after,
    ] {
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
    let aad = aead_aad(
        &row.key_id,
        &row.owner_identity,
        &row.continuation_identity,
        &row.target_fingerprint,
        &row.created_at,
        &row.expires_at,
        &row.purge_after,
    );
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::context::{PipelineContext, ProviderContinuation};
    use bitrouter_sdk::language_model::hooks::{
        ObserveHook, RequestOutcome, RouteHook, StreamHook,
    };
    use bitrouter_sdk::language_model::settlement::{
        RequiredFinalizationContext, RequiredFinalizer, SettlementContext, SettlementRecorder,
    };
    use bitrouter_sdk::language_model::{
        ApiProtocol, AppliedAuth, AuthApplier, AuthAppliers, Content, CredentialAuthority,
        FinishReason, GenerateResult, GenerationParams, HttpExecutor, Message, MockExecutor,
        MockResponse, Pipeline, PipelineBuilder, PipelineRequest, Prompt, Role, RoutingTarget,
        StaticRoutingTable, StreamAction, StreamContext, StreamInterest, StreamOutcome, StreamPart,
        Tool, ToolResultOutput, Usage,
    };
    use chrono::{TimeDelta, TimeZone, Utc};
    use futures::StreamExt;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
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
        runtime.commit(&finalization);
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
    async fn concurrent_attempt_rollback_cannot_delete_other_attempt_publication()
    -> anyhow::Result<()> {
        let registry = registry(40).await?;
        let runtime = ContinuationRuntime::new(registry.clone());
        let first = finalization_context("shared-request", 201, "provider-shared");
        let second = finalization_context("shared-request", 202, "provider-shared");
        runtime.finalize(&first).await?;
        runtime.finalize(&second).await?;
        let public_id = encode_gateway_continuation_id("shared-request")?;
        assert_eq!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Missing
        );

        runtime.rollback(&first).await?;
        assert_eq!(
            registry.resolve("owner", &public_id, Utc::now()).await?,
            ContinuationResolution::Missing,
            "one attempt cannot publish or delete state still owned by another"
        );
        runtime.commit(&second);
        runtime.rollback(&first).await?;
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
        runtime.finalize(&second).await?;
        runtime.rollback(&first).await?;
        runtime.rollback(&second).await?;

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
                .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"]))
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
                    .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"]))
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
                .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"]))
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

    struct BlockingEndHook {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        completed: Arc<AtomicUsize>,
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

    struct OutcomeObserver(Arc<std::sync::Mutex<Vec<&'static str>>>);

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
            builder.required_finalizer(blocker);
        }
        builder.required_finalizer(runtime);
        Ok(Arc::new(builder.build()?))
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

        assert!(!completed.load(Ordering::SeqCst));
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
            // The continuation DB write deliberately happens first.
            .required_finalizer(runtime)
            .required_finalizer(BlockingRequiredFinalizer {
                started: started.clone(),
                release: release.clone(),
                completed: completed.clone(),
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
        assert!(!completed.load(Ordering::SeqCst));
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
