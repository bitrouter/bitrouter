//! `ByokRouteHook` — a `language_model::RouteHook` that injects a caller's own
//! provider key into the routing chain.
//!
//! Critical invariant: when BYOK applies, it emits a
//! [`ByokKeyApplied`] event. The `byok_used` settlement flag is derived **only**
//! from that event — i.e. from the *existence of a BYOK row* — never from
//! `target.api_key_override.is_some()`. Anonymous routing / registry hooks may
//! legitimately set `api_key_override` for their own reasons; inferring BYOK
//! from it would make every such request bill free.
//!
//! This module is the only one that touches the `byok_provider_keys` table.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::language_model::{PipelineContext, RouteHook, RoutingTarget};
use bitrouter_sdk::{BitrouterError, Result};

use crate::events::ByokKeyApplied;

/// One stored BYOK provider credential.
///
/// `Debug` redacts `api_key` (v0 audit S9) so a future `tracing::error!(?cred, …)`
/// can't dump the user's upstream key to the log stream.
#[derive(Clone)]
pub struct ByokCredential {
    /// The provider this credential is for.
    pub provider: String,
    /// The caller's own API key for that provider.
    pub api_key: String,
    /// Optional API-base override that pairs with the key.
    pub api_base: Option<String>,
}

impl std::fmt::Debug for ByokCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByokCredential")
            .field("provider", &self.provider)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("api_base", &self.api_base)
            .finish()
    }
}

/// Inject caller-owned provider keys into the routing chain.
pub struct ByokRouteHook {
    pool: SqlitePool,
}

impl ByokRouteHook {
    /// Build a `ByokRouteHook` over a sqlite pool carrying this plugin's tables.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Look up an active BYOK credential for `(user_id, provider)`.
    async fn lookup(&self, user_id: &str, provider: &str) -> Result<Option<ByokCredential>> {
        let row = sqlx::query(
            "SELECT api_key, api_base FROM byok_provider_keys \
             WHERE user_id = ? AND provider = ? AND active = 1 LIMIT 1",
        )
        .bind(user_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("byok lookup: {e}")))?;
        Ok(row.map(|r| ByokCredential {
            provider: provider.to_string(),
            api_key: r.get("api_key"),
            api_base: r.get("api_base"),
        }))
    }
}

#[async_trait]
impl RouteHook for ByokRouteHook {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        let user_id = ctx.caller().user_id().to_string();
        let mut applied_providers = Vec::new();
        for target in chain.iter_mut() {
            if let Some(cred) = self.lookup(&user_id, &target.provider_name).await? {
                target.api_key_override = Some(cred.api_key);
                target.api_base_override = cred.api_base;
                applied_providers.push(target.provider_name.clone());
            }
        }
        // Emit one event per provider a BYOK row was found for. This — and
        // ONLY this — is what makes the request bill as BYOK.
        for provider in applied_providers {
            ctx.emit(ByokKeyApplied { provider });
        }
        Ok(())
    }
}

/// Insert a BYOK provider credential. Used by the CLI (`bitrouter key`) and by
/// tests.
///
/// NOTE: today the key is stored as-is. Production hardening — sealing the
/// key with an X25519 sealed-box so the database never holds plaintext
/// provider keys — is tracked as a follow-up and wired through this same
/// insert path.
pub async fn insert_byok_key(
    pool: &SqlitePool,
    id: &str,
    user_id: &str,
    provider: &str,
    api_key: &str,
    api_base: Option<&str>,
) -> Result<()> {
    // SSRF defence (v0 audit S3): a malicious BYOK row could otherwise
    // point at `http://169.254.169.254/` or `http://localhost:8200/v1/secret/`,
    // and the executor would happily POST upstream requests there with the
    // caller-provided Authorization header — exfiltrating data + IAM creds.
    if let Some(base) = api_base {
        bitrouter_sdk::url_validator::validate_upstream_url(base)?;
    }
    sqlx::query(
        "INSERT INTO byok_provider_keys \
         (id, user_id, provider, api_key, api_base, active, created_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?)",
    )
    .bind(id)
    .bind(user_id)
    .bind(provider)
    .bind(api_key)
    .bind(api_base)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .map_err(|e| BitrouterError::internal(format!("insert byok key: {e}")))?;
    Ok(())
}
