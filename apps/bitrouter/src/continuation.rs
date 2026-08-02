//! Encrypted provider continuation registry and pipeline integration.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use bitrouter_sdk::error::{BitrouterError, Result as PipelineResult};
use bitrouter_sdk::language_model::context::{PipelineContext, ProviderContinuation};
use bitrouter_sdk::language_model::hooks::RouteHook;
use bitrouter_sdk::language_model::protocol::responses::{
    decode_gateway_continuation_id, encode_gateway_continuation_id,
};
use bitrouter_sdk::language_model::settlement::{RequiredFinalizationContext, RequiredFinalizer};
use bitrouter_sdk::language_model::{ApiProtocol, RoutingTarget};
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

    fn target_fingerprint(&self, target: &RoutingTarget) -> Result<String> {
        let api_base = target
            .api_base_override
            .as_deref()
            .unwrap_or(target.api_base.as_str());
        let api_key = target
            .api_key_override
            .as_deref()
            .unwrap_or(target.api_key.as_str());
        let account = target.account_label.as_deref().unwrap_or("");
        let protocol = target.api_protocol.to_string();
        let auth_scheme = match target.auth_scheme {
            bitrouter_sdk::language_model::types::AuthScheme::XApiKey => "x-api-key",
            bitrouter_sdk::language_model::types::AuthScheme::Bearer => "bearer",
        };
        let digest = hmac_bytes(
            &self.secret,
            TARGET_FINGERPRINT_DOMAIN,
            &[
                target.provider_name.as_str(),
                account,
                protocol.as_str(),
                api_base,
                auth_scheme,
                api_key,
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
    pub fn matches_target(&self, target: &RoutingTarget) -> Result<bool> {
        Ok(self.key.target_fingerprint(target)? == self.target_fingerprint)
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
        })
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.db
    }

    pub async fn bind(
        &self,
        owner_user_id: &str,
        gateway_request_id: &str,
        provider_response_id: &str,
        target: &RoutingTarget,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let key = self.keys.load()?;
        self.ensure_key_epoch(&key, now).await?;
        let owner_identity = key.owner_identity(owner_user_id)?;
        let continuation_identity = key.continuation_identity(owner_user_id, gateway_request_id)?;
        let target_fingerprint = key.target_fingerprint(target)?;
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
            Ok(_) => Ok(()),
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
                    Ok(())
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
}

impl ContinuationRuntime {
    pub fn new(registry: ContinuationRegistry) -> Self {
        Self { registry }
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
                    if active.matches_target(target).map_err(|error| {
                        BitrouterError::internal(format!(
                            "validating provider continuation target: {error}"
                        ))
                    })? {
                        selected = Some(target.clone());
                        break;
                    }
                }
                let selected = selected.ok_or_else(|| {
                    BitrouterError::bad_request(
                        "provider continuation target is unavailable or changed",
                    )
                })?;
                chain.clear();
                chain.push(selected.clone());
                ctx.insert_extension(Arc::new(ProviderContinuation::new(
                    active.provider_response_id,
                    &selected,
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
        let public_continuation_id = encode_gateway_continuation_id(&ctx.request_id)?;
        let now = Utc::now();
        self.registry.prune(now).await.map_err(|error| {
            BitrouterError::internal(format!("pruning provider continuations: {error}"))
        })?;
        self.registry
            .bind(
                ctx.caller.user_id(),
                &public_continuation_id,
                provider_response_id,
                target,
                now,
            )
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("persisting provider continuation: {error}"))
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
    use async_trait::async_trait;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::context::{PipelineContext, ProviderContinuation};
    use bitrouter_sdk::language_model::hooks::RouteHook;
    use bitrouter_sdk::language_model::settlement::{
        RequiredFinalizationContext, RequiredFinalizer,
    };
    use bitrouter_sdk::language_model::{
        ApiProtocol, GenerationParams, HttpExecutor, Message, MockExecutor, MockResponse,
        PipelineBuilder, PipelineRequest, Prompt, Role, RoutingTarget, StaticRoutingTable,
        StreamPart, Tool, ToolResultOutput,
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
        assert!(active.matches_target(&target("credential-a"))?);
        assert!(!active.matches_target(&target("credential-b"))?);
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
        let bind = || registry.bind("owner", "gateway", "resp-same", &bound_target, now);
        let (left, right) = tokio::join!(bind(), bind());
        left?;
        right?;
        let collision = registry
            .bind(
                "owner",
                "gateway",
                "resp-different",
                &target("credential"),
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
            .bind("owner", &public_id, "resp-final", &bound, now)
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
            .bind("owner", &public_id, "resp-final", &bound, Utc::now())
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
        runtime
            .finalize(&RequiredFinalizationContext {
                request_id: "gateway-result".into(),
                caller: CallerContext::new("key", "owner"),
                target: Some(target("credential")),
                inbound_protocol: Some(ApiProtocol::Responses),
                response_id: Some("resp-provider-final".into()),
                finish_reason: Some(bitrouter_sdk::language_model::FinishReason::Stop),
                streamed: true,
                successful_terminal: true,
            })
            .await?;
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

    fn tool_request(request_id: &str, previous_response_id: Option<&str>) -> PipelineRequest {
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
                stream: true,
            },
            inbound_protocol: Some(ApiProtocol::Responses),
        }
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
            StreamPart::Finish {
                reason: bitrouter_sdk::language_model::FinishReason::Stop,
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
