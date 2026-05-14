//! `AuthHook` — the `language_model::PreRequestHook` that authenticates a
//! request against a `brvk_` virtual key.
//!
//! v1 has **no JWT path** (004 §3.0): the only credential form is a virtual
//! key, looked up by SHA-256 hash in the `api_keys` table.
//!
//! Relationship to `server.skip_auth` (004 §3.4): `skip_auth` is an SDK-level
//! flag handled at the server entry — when it is on and a request carries no
//! credentials, the server synthesises a *local* `CallerContext`. `AuthHook`
//! respects an already-local caller and lets it through. The four-way truth
//! table (skip_auth × has-credential):
//!
//! | skip_auth | credential | result                                |
//! |-----------|------------|---------------------------------------|
//! | false     | present    | validated (Allow / Deny)              |
//! | false     | absent     | Deny 401                              |
//! | true      | present    | validated (credentials still checked) |
//! | true      | absent     | Allow — local caller passes through   |

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::SqlitePool;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::{MppVerifier, PluginId, Result};

use crate::db::{self, ApiKeyRecord};
use crate::events::{Authenticated, MppVerified};
use crate::keys;

/// The auth plugin id, used as the `PipelineContext` metadata key.
pub fn plugin_id() -> PluginId {
    PluginId::new("bitrouter-auth")
}

/// Authenticates a request against the `api_keys` table (a `brvk_` virtual
/// key) or an MPP `Payment-SIGNATURE` credential. Owns no routing or
/// settlement behaviour — it only establishes identity.
pub struct AuthHook {
    pool: SqlitePool,
    /// Optional MPP credential verifier (004 §3.1). Without it, a
    /// `Payment-SIGNATURE` request is rejected with 402 rather than verified.
    /// `MppVerifier` is implemented by `bitrouter-settlement::MppState`.
    mpp_verifier: Option<Arc<dyn MppVerifier>>,
}

impl AuthHook {
    /// Build an `AuthHook` over a sqlite pool. The pool must already have this
    /// plugin's tables (`crate::db::migrate`).
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            mpp_verifier: None,
        }
    }

    /// Attach an MPP credential verifier, enabling the `Payment-SIGNATURE`
    /// authentication path (builder-style).
    pub fn with_mpp_verifier(mut self, verifier: Arc<dyn MppVerifier>) -> Self {
        self.mpp_verifier = Some(verifier);
        self
    }

    /// Extract a presented API-key credential from the request headers. Both
    /// the OpenAI-style `Authorization: Bearer …` and the Anthropic-style
    /// `x-api-key: …` headers are accepted.
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

    /// Extract the MPP `Payment-SIGNATURE` header value, if present.
    fn extract_payment_credential(ctx: &PipelineContext) -> Option<String> {
        ctx.headers()
            .get("payment-signature")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// Verify an MPP payment credential and, on success, establish an
    /// MPP-funded caller (004 §3.1 / §3.3).
    async fn verify_mpp(
        &self,
        credential: &str,
        ctx: &mut PipelineContext,
    ) -> Result<HookDecision> {
        let Some(verifier) = &self.mpp_verifier else {
            // The server has no MPP verifier wired — an MPP credential cannot
            // be honoured here.
            return Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                "MPP payment is not enabled on this server".to_string(),
            )));
        };
        match verifier.verify(credential).await? {
            Some(verified) => {
                let caller = CallerContext::new(
                    &verified.session_id,
                    &verified.user_id,
                    bitrouter_sdk::caller::PaymentMethod::Mpp,
                );
                ctx.set_caller(caller);
                ctx.set_metadata(
                    &plugin_id(),
                    serde_json::json!({
                        "api_key_id": verified.session_id,
                        "user_id": verified.user_id,
                        "policy_id": serde_json::Value::Null,
                    }),
                );
                ctx.emit(Authenticated {
                    api_key_id: verified.session_id.clone(),
                    user_id: verified.user_id,
                    payment_method: bitrouter_sdk::caller::PaymentMethod::Mpp,
                    policy_id: None,
                });
                ctx.emit(MppVerified {
                    session_id: verified.session_id,
                    channel_balance: verified.channel_balance_micro_usd,
                });
                Ok(HookDecision::Allow)
            }
            None => Ok(HookDecision::Deny(DenyReason::PaymentRequired(
                "MPP credential not recognised or channel exhausted".to_string(),
            ))),
        }
    }

    /// Turn a validated key record into a `CallerContext`.
    fn caller_from_record(record: &ApiKeyRecord) -> CallerContext {
        let mut caller = CallerContext::new(&record.id, &record.user_id, record.payment_method);
        if let Some(limit) = record.spend_limit_micro_usd {
            caller = caller.with_spend_limit(limit.max(0) as u64);
        }
        if let Some(rpm) = record.rpm_limit {
            caller = caller.with_rpm_limit(rpm.max(0) as u32);
        }
        caller
    }
}

#[async_trait]
impl PreRequestHook for AuthHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let credential = Self::extract_credential(ctx);
        let payment_credential = Self::extract_payment_credential(ctx);

        // Mutual exclusion (004 §3.1): an API key and an MPP payment credential
        // must not both be presented.
        if credential.is_some() && payment_credential.is_some() {
            return Ok(HookDecision::Deny(DenyReason::BadRequest(
                "both an API key and an MPP payment credential were provided".to_string(),
            )));
        }

        // MPP path (004 §3.1 / §3.3): a `Payment-SIGNATURE` credential.
        if let Some(payment_credential) = payment_credential {
            return self.verify_mpp(&payment_credential, ctx).await;
        }

        // API-key path.
        let Some(credential) = credential else {
            // No credential. Admit only the skip_auth-synthesised local caller.
            if ctx.caller().is_local() {
                return Ok(HookDecision::Allow);
            }
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
        if let Some(expires_at) = record.expires_at {
            if expires_at <= Utc::now() {
                return Ok(HookDecision::Deny(DenyReason::Unauthorized(
                    "API key has expired".to_string(),
                )));
            }
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
            payment_method: record.payment_method,
            policy_id: record.policy_id,
        });
        Ok(HookDecision::Allow)
    }
}
