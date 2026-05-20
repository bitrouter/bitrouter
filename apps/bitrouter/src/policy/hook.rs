//! `PolicyHook` — a `language_model::PreRequestHook` enforcing per-API-key
//! policy: model allow/deny, spend ceiling, expiry, tool-access rules,
//! request-rate limits.
//!
//! The caller's `policy_id` is read from the auth module's `PipelineContext`
//! metadata (not by importing the auth crate's event type). Spend and rate
//! are read from the sibling [`crate::metering::MeteringStore`] — a direct
//! concrete-type call, no SDK trait in between.

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::PluginId;
use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};

use crate::metering::{MeteringStore, TimeWindow};
use crate::policy::store::PolicyStore;

/// Enforces per-API-key policy at Stage 1.
pub struct PolicyHook {
    store: Arc<PolicyStore>,
    /// Optional — without a [`MeteringStore`], spend / rate ceilings cannot
    /// be enforced (model allow/deny, expiry, and tool-access checks still
    /// are).
    metering: Option<MeteringStore>,
}

impl PolicyHook {
    /// Build a `PolicyHook` over a policy store, optionally with a metering
    /// store for spend + rate enforcement.
    pub fn new(store: Arc<PolicyStore>, metering: Option<MeteringStore>) -> Self {
        Self { store, metering }
    }

    /// Read the caller's `policy_id` from the auth module's context metadata.
    fn policy_id(ctx: &PipelineContext) -> Option<String> {
        ctx.get_metadata(&PluginId::new("bitrouter-auth"))
            .and_then(|m| m.get("policy_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

#[async_trait]
impl PreRequestHook for PolicyHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let policy_id = Self::policy_id(ctx);
        // No policy bound → no constraints (the combination is permissive).
        let ids: Vec<&str> = policy_id.as_deref().into_iter().collect();
        let effective = self.store.effective_for(&ids);

        // 1. model allow / deny
        if let Err(violation) = effective.check_model(ctx.model()) {
            return Ok(HookDecision::Deny(DenyReason::Forbidden(
                violation.to_string(),
            )));
        }

        // 2. hard expiry
        if let Err(violation) = effective.check_expiry(chrono::Utc::now()) {
            return Ok(HookDecision::Deny(DenyReason::Forbidden(
                violation.to_string(),
            )));
        }

        // 3. tool-access rules — checked against the request's declared tools
        if effective.has_tool_restriction() {
            let tools = ctx.prompt().tools.iter().map(|t| t.name.as_str());
            if let Err(violation) = effective.check_tools(tools) {
                return Ok(HookDecision::Deny(DenyReason::Forbidden(
                    violation.to_string(),
                )));
            }
        }

        // 4. spend ceiling — only enforceable with a MeteringStore
        if effective.max_spend_micro_usd.is_some()
            && let Some(metering) = &self.metering
        {
            let spent = metering
                .get_spend(ctx.caller().api_key_id(), TimeWindow::ThisMonth)
                .await?;
            if let Err(violation) = effective.check_spend(spent) {
                return Ok(HookDecision::Deny(DenyReason::Forbidden(
                    violation.to_string(),
                )));
            }
        }

        // 5. request-rate ceiling — also reads the MeteringStore. A rate
        //    violation maps to 429 (RateLimited) rather than 403, with a
        //    Retry-After hint.
        if effective.max_requests_per_minute.is_some()
            && let Some(metering) = &self.metering
        {
            let rate = metering.get_rate(ctx.caller().api_key_id()).await?;
            let observed = rate.requests_per_minute.round().max(0.0) as u32;
            if let Err(violation) = effective.check_rate(observed) {
                tracing::debug!(%violation, "policy rate limit hit");
                return Ok(HookDecision::Deny(DenyReason::RateLimited {
                    retry_after: Some(60),
                }));
            }
        }

        Ok(HookDecision::Allow)
    }
}
