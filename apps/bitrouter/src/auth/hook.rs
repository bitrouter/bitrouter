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

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::DatabaseConnection;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::{PluginId, Result};
use http::HeaderMap;

use crate::acp_runtime::AcpRuntime;
use crate::auth::db::{self, ApiKeyRecord};
use crate::auth::events::{Authenticated, ControllerAuthenticated};
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
    db: DatabaseConnection,
    acp_runtime: Option<Arc<AcpRuntime>>,
}

impl AuthHook {
    /// Build an `AuthHook` over a database connection. The database must
    /// already carry this module's tables (`crate::db::run_migrations`).
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            acp_runtime: None,
        }
    }

    /// Build an auth hook that also recognizes daemon-issued ACP controller
    /// credentials before the local skip-auth compatibility branch.
    pub fn with_acp_runtime(db: DatabaseConnection, acp_runtime: Arc<AcpRuntime>) -> Self {
        Self {
            db,
            acp_runtime: Some(acp_runtime),
        }
    }

    /// Extract a presented API-key credential from the request headers.
    /// Both the OpenAI-style `Authorization: Bearer …` and the
    /// Anthropic-style `x-api-key: …` headers are accepted.
    fn extract_credential(ctx: &PipelineContext) -> Option<String> {
        credential_from_headers(ctx.headers())
    }

    /// Turn a validated key record into a `CallerContext`.
    fn caller_from_record(record: &ApiKeyRecord) -> CallerContext {
        CallerContext::new(&record.id, &record.user_id)
    }
}

/// Shared credential extraction for native inference and app-owned control
/// plane endpoints.
pub(crate) fn credential_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth).trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[async_trait]
impl PreRequestHook for AuthHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let credential = Self::extract_credential(ctx);
        if let (Some(runtime), Some(credential)) = (&self.acp_runtime, credential.as_deref())
            && let Some(principal) = runtime.authenticate(credential)
        {
            let controller_instance_id = principal.controller_instance_id().to_string();
            ctx.set_caller(CallerContext::new_scoped(
                "acp-controller",
                "acp-controller",
                controller_instance_id.clone(),
            ));
            ctx.set_metadata(
                &plugin_id(),
                serde_json::json!({
                    "api_key_id": "acp-controller",
                    "user_id": "acp-controller",
                    "policy_id": null,
                    "controller_instance_id": controller_instance_id,
                }),
            );
            ctx.emit(Authenticated {
                api_key_id: "acp-controller".to_string(),
                user_id: "acp-controller".to_string(),
                policy_id: None,
            });
            ctx.emit(ControllerAuthenticated {
                controller_instance_id,
                expires_at: principal.expires_at().to_rfc3339(),
            });
            return Ok(HookDecision::Allow);
        }

        // `skip_auth=true` on the SDK side synthesises a local caller
        // for *every* inbound request — admit immediately regardless of
        // any presented header. Validating a stray `Authorization`
        // bearer would otherwise reject zero-config clients that
        // auto-inject a placeholder token (Claude Code, litellm, …).
        if ctx.caller().is_local() {
            return Ok(HookDecision::Allow);
        }

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
        let record = db::find_key_by_hash(&self.db, &hash).await?;
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
