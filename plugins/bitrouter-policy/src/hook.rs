//! `PolicyHook` — a `language_model::PreRequestHook` enforcing per-API-key
//! policy: model allow/deny, spend ceiling, expiry, payment-chain limits,
//! tool-access rules and request-rate limits (004 §4.1).
//!
//! The caller's `policy_id` is read from the `bitrouter-auth` plugin's
//! `PipelineContext` metadata (not by importing the auth crate's event type) —
//! see 003 §3.3. Spend and rate are read from the injected `MetricsStore`
//! (003 §4.7.3).

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::Result;
use bitrouter_sdk::caller::PaymentMethod;
use bitrouter_sdk::language_model::{DenyReason, HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::metrics::{MetricsStore, TimeWindow};

use crate::store::PolicyStore;

/// Enforces per-API-key policy at Stage 1.
pub struct PolicyHook {
    store: Arc<PolicyStore>,
    /// Optional — without a `MetricsStore`, spend ceilings cannot be enforced
    /// (model allow/deny and expiry still are).
    metrics_store: Option<Arc<dyn MetricsStore>>,
}

impl PolicyHook {
    /// Build a `PolicyHook` over a policy store, optionally with a metrics
    /// store for spend enforcement.
    pub fn new(store: Arc<PolicyStore>, metrics_store: Option<Arc<dyn MetricsStore>>) -> Self {
        Self {
            store,
            metrics_store,
        }
    }

    /// Read the caller's `policy_id` from the auth plugin's context metadata.
    fn policy_id(ctx: &PipelineContext) -> Option<String> {
        ctx.get_metadata(&bitrouter_sdk::PluginId::new("bitrouter-auth"))
            .and_then(|m| m.get("policy_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// The caller's payment chain, for chain-limit checks. v1.0 supports the
    /// Tempo MPP channel only (008 §1.1), so an MPP caller is on `tempo`;
    /// non-MPP callers have no chain to gate.
    fn caller_chain(ctx: &PipelineContext) -> Option<&'static str> {
        match ctx.caller().payment_method() {
            PaymentMethod::Mpp => Some("tempo"),
            _ => None,
        }
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

        // 3. payment-chain limit — only meaningful for chain-funded (MPP) callers
        if effective.has_chain_restriction() {
            if let Some(chain) = Self::caller_chain(ctx) {
                if let Err(violation) = effective.check_chain(chain) {
                    return Ok(HookDecision::Deny(DenyReason::Forbidden(
                        violation.to_string(),
                    )));
                }
            }
        }

        // 4. tool-access rules — checked against the request's declared tools
        if effective.has_tool_restriction() {
            let tools = ctx.prompt().tools.iter().map(|t| t.name.as_str());
            if let Err(violation) = effective.check_tools(tools) {
                return Ok(HookDecision::Deny(DenyReason::Forbidden(
                    violation.to_string(),
                )));
            }
        }

        // 5. spend ceiling — only enforceable with a MetricsStore
        if effective.max_spend_micro_usd.is_some() {
            if let Some(metrics) = &self.metrics_store {
                let spent = metrics
                    .get_spend(ctx.caller().api_key_id(), TimeWindow::ThisMonth)
                    .await?;
                if let Err(violation) = effective.check_spend(spent) {
                    return Ok(HookDecision::Deny(DenyReason::Forbidden(
                        violation.to_string(),
                    )));
                }
            }
        }

        // 6. request-rate ceiling — also reads the MetricsStore. A rate
        //    violation maps to 429 (RateLimited) rather than 403, with a
        //    Retry-After hint.
        if effective.max_requests_per_minute.is_some() {
            if let Some(metrics) = &self.metrics_store {
                let rate = metrics.get_rate(ctx.caller().api_key_id()).await?;
                let observed = rate.requests_per_minute.round().max(0.0) as u32;
                if let Err(violation) = effective.check_rate(observed) {
                    tracing::debug!(%violation, "policy rate limit hit");
                    return Ok(HookDecision::Deny(DenyReason::RateLimited {
                        retry_after: Some(60),
                    }));
                }
            }
        }

        Ok(HookDecision::Allow)
    }
}
