//! `AuthHook` — the `language_model::PreRequestHook` that authenticates a
//! request against a `brvk_` virtual key.
//!
//! v1 has **no JWT path**: the only credential form is a virtual key,
//! looked up by SHA-256 hash in the `api_keys` table.
//!
//! Relationship to `server.skip_auth`: `skip_auth` is an SDK-level flag
//! handled at the server entry — when it is on, the server synthesises
//! a *local* `CallerContext` for every inbound request, **regardless of
//! any credential header**. The intent is "local-first, fully open" —
//! the same posture the zero-config startup uses to make tools like
//! codex / Claude Code / litellm just work without requiring the user
//! to mint a virtual key first. The four-way truth table:
//!
//! | skip_auth | credential | result                                |
//! |-----------|------------|---------------------------------------|
//! | false     | present    | validated (Allow / Deny)              |
//! | false     | absent     | Deny 401                              |
//! | true      | present    | Allow — any header accepted as local  |
//! | true      | absent     | Allow — local caller passes through   |
//!
//! Previously the `true × present` row also validated — which silently
//! broke Claude Code and litellm because both auto-inject an
//! `Authorization: Bearer …` header that bitrouter saw as a malformed
//! virtual key. Multi-tenant operators set `skip_auth: false` (the SDK
//! default) to get real validation.

use async_trait::async_trait;
use chrono::Utc;
use sqlx::SqlitePool;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::{PluginId, Result};

use crate::auth::db::{self, ApiKeyRecord};
use crate::auth::events::Authenticated;
use crate::auth::keys;

/// The auth module id, used as the `PipelineContext` metadata key. The
/// string is preserved as `bitrouter-auth` so policy code that reads
/// metadata under that key continues to work after the move from a
/// shared plugin into a binary module.
pub fn plugin_id() -> PluginId {
    PluginId::new("bitrouter-auth")
}

/// Authenticates a request against the `api_keys` table (a `brvk_`
/// virtual key). Owns no routing or settlement behaviour — it only
/// establishes identity.
pub struct AuthHook {
    pool: SqlitePool,
}

impl AuthHook {
    /// Build an `AuthHook` over a sqlite pool. The pool must already have
    /// this module's tables (`crate::auth::db::migrate`).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Extract a presented API-key credential from the request headers.
    /// Both the OpenAI-style `Authorization: Bearer …` and the
    /// Anthropic-style `x-api-key: …` headers are accepted.
    fn extract_credential(ctx: &PipelineContext) -> Option<String> {
        let headers = ctx.headers();
        if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
            let token = auth.strip_prefix("Bearer ").unwrap_or(auth).trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
        if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
            let key = key.trim();
            if !key.is_empty() {
                return Some(key.to_string());
            }
        }
        None
    }

    /// Turn a validated key record into a `CallerContext`.
    fn caller_from_record(record: &ApiKeyRecord) -> CallerContext {
        CallerContext::new(&record.id, &record.user_id)
    }
}

#[async_trait]
impl PreRequestHook for AuthHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        // `skip_auth=true` on the SDK side synthesises a local caller
        // for *every* inbound request — admit immediately regardless of
        // any presented header. Validating a stray `Authorization`
        // bearer would otherwise reject zero-config clients that
        // auto-inject a placeholder token (Claude Code, litellm, …).
        if ctx.caller().is_local() {
            return Ok(HookDecision::Allow);
        }

        let credential = Self::extract_credential(ctx);

        // API-key path.
        let Some(credential) = credential else {
            return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                "missing API key".to_string(),
            )));
        };

        // v1 has no JWT path — the credential must be a `brvk_` virtual key.
        if !keys::looks_like_virtual_key(&credential) {
            return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                "credential is not a brvk_ virtual key".to_string(),
            )));
        }

        let hash = keys::hash_key(&credential);
        let record = db::find_key_by_hash(&self.pool, &hash).await?;
        let Some(record) = record else {
            return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                "unknown API key".to_string(),
            )));
        };

        if !record.active {
            return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                "API key is inactive".to_string(),
            )));
        }
        if let Some(expires_at) = record.expires_at
            && expires_at <= Utc::now()
        {
            return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                "API key has expired".to_string(),
            )));
        }

        // Establish identity: upgrade the pre-auth caller and broadcast it.
        let caller = Self::caller_from_record(&record);
        ctx.set_caller(caller);
        ctx.set_metadata(
            &plugin_id(),
            serde_json::json!({
                "api_key_id": record.id,
                "user_id": record.user_id,
                "policy_id": record.policy_id,
            }),
        );
        ctx.emit(Authenticated {
            api_key_id: record.id,
            user_id: record.user_id,
            policy_id: record.policy_id,
        });
        Ok(HookDecision::Allow)
    }
}
