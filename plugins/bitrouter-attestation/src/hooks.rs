//! [`AttestationRouteHook`] — a Stage-2 [`RouteHook`] that resolves a TEE
//! attestation verdict for each confidential routing target and either tags it
//! (Record) or drops unverified targets (Enforce).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bitrouter_attestation::AttestationVerdict;
use bitrouter_sdk::PluginId;
use bitrouter_sdk::Result;
use bitrouter_sdk::language_model::{PipelineContext, RouteHook, RoutingTarget};

use crate::{AttestationConfig, AttestationPolicy};

/// Request-scoped attestation result deposited into the pipeline's typed
/// extensions so downstream stages (receipt, response annotation) can surface
/// it. Holds one verdict per distinct confidential `(provider, model)` routed.
#[derive(Debug, Clone)]
pub struct AttestationOutcome {
    pub verdicts: Vec<AttestationVerdict>,
}

/// Stage-2 route hook. See module docs.
pub struct AttestationRouteHook {
    config: AttestationConfig,
}

impl AttestationRouteHook {
    pub fn new(config: AttestationConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl RouteHook for AttestationRouteHook {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<()> {
        let now = now_unix();

        // Resolve one verdict per distinct confidential (provider, model) in the
        // chain. A registry miss or verifier error is treated as UNVERIFIED —
        // fail-closed (spec §1.5 cond. 3), never a silent pass.
        let mut verdicts: Vec<AttestationVerdict> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for target in chain.iter() {
            if !self.config.is_confidential(&target.provider_name) {
                continue;
            }
            if !seen.insert((target.provider_name.clone(), target.service_id.clone())) {
                continue;
            }
            let verdict = match self.config.registry.get(&target.provider_name) {
                Ok(verifier) => verifier
                    .attestation_cached(&target.service_id, now)
                    .await
                    .unwrap_or_else(|_| {
                        AttestationVerdict::unverified(
                            target.service_id.clone(),
                            String::new(),
                            now,
                        )
                    }),
                Err(_) => {
                    AttestationVerdict::unverified(target.service_id.clone(), String::new(), now)
                }
            };
            verdicts.push(verdict);
        }

        if verdicts.is_empty() {
            return Ok(()); // no confidential targets — nothing to do
        }

        // Record the verdicts for downstream stages regardless of policy.
        if let Ok(value) = serde_json::to_value(&verdicts) {
            ctx.set_metadata(
                &plugin_id(),
                serde_json::json!({
                    "policy": self.config.policy.label(),
                    "verdicts": value,
                }),
            );
        }
        ctx.insert_extension(Arc::new(AttestationOutcome {
            verdicts: verdicts.clone(),
        }));

        // Enforce: drop confidential targets that didn't verify. Non-confidential
        // targets are always kept.
        if self.config.policy == AttestationPolicy::Enforce {
            chain.retain(|target| {
                if !self.config.is_confidential(&target.provider_name) {
                    return true;
                }
                verdicts
                    .iter()
                    .any(|v| v.model == target.service_id && v.verified)
            });
        }

        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn plugin_id() -> PluginId {
    PluginId::new("bitrouter-attestation")
}
