//! Request-owned protocol tariffs captured after final route mutations.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_ai::types::{ApiProtocol, Usage, UsageOrigin};
use bitrouter_sdk::event::PipelineEvent;
use bitrouter_sdk::model_call::context::PipelineContext;
use bitrouter_sdk::model_call::hooks::RouteHook;
use bitrouter_sdk::model_call::settlement::SettlementContext;
use bitrouter_sdk::model_call::stream::{
    PricingTargetKey, UsagePricing, UsagePricingBracket, UsagePricingSnapshot, UsagePricingTier,
};
use bitrouter_sdk::model_call::types::RoutingTarget;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::pricing::{
    ChargeEvidence, ModelPricing, PricingSource, PricingTable, calculate_charge_evidence,
    calculate_total_input_charge_evidence, pricing_version, systemone_input_only_rates,
    unavailable_charge_evidence,
};

/// Full tariff and applicability frozen for one effective target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenTariff {
    /// Wire actually selected for dispatch.
    pub protocol: ApiProtocol,
    /// Effective endpoint profile, with custom URLs represented only by a digest.
    pub endpoint_profile: String,
    /// Profile to which the admitted rates were bound; absent in older evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tariff_profile: Option<String>,
    /// Complete independent rates and context tiers, when applicable.
    pub pricing: Option<ModelPricing>,
    /// Digest covering wire, profile, rates/tiers and the billing condition.
    pub pricing_version: String,
    /// Existing normalized usage contract, or Decisions' zero-cache-only gate.
    pub billing_basis: String,
    /// Content-free applicability failure, if any.
    pub unavailable_reason: Option<String>,
}

impl FrozenTariff {
    /// Apply only this admitted tariff, retaining the snapshot even on failure.
    pub fn charge_evidence(&self, usage: &Usage, source: PricingSource) -> ChargeEvidence {
        let reason = if usage.origin == UsageOrigin::Unknown {
            Some("usage_unavailable")
        } else if let Some(reason) = self.unavailable_reason.as_deref() {
            Some(reason)
        } else if self.protocol == ApiProtocol::Decisions
            && (usage.cache_read_tokens != 0 || usage.cache_write_tokens != 0)
        {
            Some("decisions_cache_billing_unverified")
        } else {
            None
        };
        let mut evidence = match (reason, &self.pricing) {
            (Some(reason), _) => unavailable_charge_evidence(usage, reason),
            (None, Some(pricing)) if self.protocol == ApiProtocol::SystemOne => {
                calculate_total_input_charge_evidence(usage, pricing, source)
            }
            (None, Some(pricing)) => calculate_charge_evidence(usage, pricing, source),
            (None, None) => unavailable_charge_evidence(usage, "pricing_not_found"),
        };
        evidence.pricing_version.clone_from(&self.pricing_version);
        evidence.tariff_snapshot = Some(self.clone());
        evidence
    }

    /// Admission coverage for every possible usage condition. A zero cache
    /// count observed on a previous request cannot establish native cache billing.
    pub fn guarantees_known_price(&self) -> bool {
        if self.protocol == ApiProtocol::Decisions || self.unavailable_reason.is_some() {
            return false;
        }
        self.pricing.as_ref().is_some_and(|pricing| {
            let applicable = |pricing: &ModelPricing| {
                if self.protocol == ApiProtocol::SystemOne {
                    systemone_input_only_rates(pricing)
                } else {
                    complete_rates(pricing)
                }
            };
            applicable(pricing)
                && pricing.context_tiers.iter().all(|tier| {
                    applicable(
                        &pricing
                            .resolve_for_input_tokens(tier.above_input_tokens.saturating_add(1)),
                    )
                })
        })
    }
}

fn complete_rates(pricing: &ModelPricing) -> bool {
    [
        pricing.input_micro_usd_per_token,
        pricing.cache_read_micro_usd_per_token,
        pricing.cache_write_micro_usd_per_token,
        pricing.output_micro_usd_per_token,
    ]
    .into_iter()
    .all(|rate| rate.is_some_and(|rate| rate.is_finite() && rate >= 0.0))
}

/// Explicit offline prices retain the admission evidence and cannot resolve
/// the native cache billing basis. The source and effective rates distinguish
/// the override calculation from the original frozen tariff.
pub(crate) fn override_charge_evidence(
    usage: &Usage,
    pricing: &ModelPricing,
    admitted: Option<&FrozenTariff>,
) -> ChargeEvidence {
    let mut evidence = if admitted.is_some_and(|tariff| tariff.protocol == ApiProtocol::Decisions)
        && (usage.cache_read_tokens != 0 || usage.cache_write_tokens != 0)
    {
        unavailable_charge_evidence(usage, "decisions_cache_billing_unverified")
    } else if admitted.is_some_and(|tariff| tariff.protocol == ApiProtocol::SystemOne) {
        calculate_total_input_charge_evidence(usage, pricing, PricingSource::Override)
    } else {
        calculate_charge_evidence(usage, pricing, PricingSource::Override)
    };
    if let Some(admitted) = admitted {
        evidence.pricing_version = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(
                format!(
                    "override-v1|{}|{}",
                    admitted.pricing_version, evidence.pricing_version
                )
                .as_bytes()
            ))
        );
        evidence.tariff_snapshot = Some(admitted.clone());
    }
    evidence
}

/// Typed evidence linking a tariff to the exact admitted endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct TargetTariffSnapshot {
    pub target: PricingTargetKey,
    pub tariff: FrozenTariff,
}
impl PipelineEvent for TargetTariffSnapshot {
    fn event_name(&self) -> &'static str {
        "pricing.tariff_snapshot"
    }
}

impl PricingTable {
    /// Freeze the configured rates and the effective endpoint's applicability.
    pub fn snapshot(&self, target: &RoutingTarget) -> TargetTariffSnapshot {
        let profile = endpoint_profile(target.effective_api_base());
        let pricing = self.resolve(
            &target.provider_name,
            &target.service_id,
            &target.api_protocol,
        );
        let configured = pricing
            .as_ref()
            .and_then(|pricing| pricing.endpoint_profile)
            .map(|profile| profile.as_str())
            .or_else(|| self.endpoint_profile_for(&target.provider_name, &target.api_protocol));
        let reason = match configured {
            _ if profile == "invalid_endpoint" => Some("endpoint_profile_unavailable"),
            None => Some("endpoint_profile_unavailable"),
            Some(expected) if expected != profile => Some("endpoint_profile_mismatch"),
            Some(_)
                if pricing
                    .as_ref()
                    .is_none_or(|pricing| pricing.is_unconfigured()) =>
            {
                Some("pricing_not_found")
            }
            Some(_) => None,
        };
        let basis = if target.api_protocol == ApiProtocol::SystemOne {
            "systemone-total-input-v1"
        } else if target.api_protocol == ApiProtocol::Decisions {
            "decisions-zero-cache-only-v1"
        } else {
            "normalized-disjoint-usage-v1"
        };
        let rates_version = pricing
            .as_ref()
            .map(pricing_version)
            .unwrap_or_else(|| "unavailable".into());
        let version = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(
                format!(
                    "tariff-v2|{}|{profile}|{configured:?}|{basis}|{rates_version}|{}",
                    target.api_protocol,
                    reason.unwrap_or("available")
                )
                .as_bytes()
            ))
        );
        TargetTariffSnapshot {
            target: PricingTargetKey::from_target(target),
            tariff: FrozenTariff {
                protocol: target.api_protocol.clone(),
                endpoint_profile: profile,
                tariff_profile: configured.map(str::to_owned),
                pricing,
                pricing_version: version,
                billing_basis: basis.into(),
                unavailable_reason: reason.map(str::to_owned),
            },
        }
    }
}

/// Recognized OpenAI processing profiles; every other configured endpoint is
/// pinned by digest. No regional premium is inferred or applied here.
pub(crate) fn endpoint_profile(api_base: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(api_base) else {
        return "invalid_endpoint".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if url.scheme() == "https"
        && url.port().is_none_or(|port| port == 443)
        && url.path().trim_end_matches('/') == "/v1"
        && url.query().is_none()
    {
        match url.host_str() {
            Some("api.openai.com") => return "openai_global".into(),
            Some("us.api.openai.com") => return "openai_us".into(),
            Some("eu.api.openai.com") => return "openai_europe".into(),
            _ => {}
        }
    }
    format!(
        "configured:sha256:{}",
        hex::encode(Sha256::digest(url.as_str().as_bytes()))
    )
}

/// Host-owned capture at the pipeline's read-only final-route boundary.
pub struct CaptureTariffs {
    pricing: Arc<PricingTable>,
    require_known: bool,
}
impl CaptureTariffs {
    pub fn new(pricing: Arc<PricingTable>, require_known: bool) -> Self {
        Self {
            pricing,
            require_known,
        }
    }
}
#[async_trait]
impl RouteHook for CaptureTariffs {
    async fn after_resolve(
        &self,
        chain: &[RoutingTarget],
        ctx: &mut PipelineContext,
    ) -> bitrouter_sdk::Result<()> {
        let snapshots = chain
            .iter()
            .map(|target| self.pricing.snapshot(target))
            .collect::<Vec<_>>();
        if self.require_known
            && snapshots
                .iter()
                .any(|snapshot| !snapshot.tariff.guarantees_known_price())
        {
            return Err(bitrouter_sdk::error::BitrouterError::bad_request(
                "known-price coverage is unavailable for the selected route",
            ));
        }
        for snapshot in snapshots {
            let pricing = if snapshot.tariff.unavailable_reason.is_none() {
                snapshot.tariff.pricing.as_ref().map(stream_pricing)
            } else {
                None
            };
            ctx.emit(UsagePricingSnapshot {
                target: snapshot.target.clone(),
                pricing,
            });
            ctx.emit(snapshot);
        }
        Ok(())
    }
}

fn stream_pricing(pricing: &ModelPricing) -> UsagePricing {
    let bracket = |pricing: &ModelPricing| UsagePricingBracket {
        input_micro_usd_per_token: pricing.input_micro_usd_per_token,
        cache_read_micro_usd_per_token: pricing.cache_read_micro_usd_per_token,
        cache_write_micro_usd_per_token: pricing.cache_write_micro_usd_per_token,
        output_micro_usd_per_token: pricing.output_micro_usd_per_token,
        reasoning_output_micro_usd_per_token: None,
    };
    UsagePricing {
        base: bracket(pricing),
        context_tiers: pricing
            .context_tiers
            .iter()
            .map(|tier| UsagePricingTier {
                above_input_tokens: tier.above_input_tokens,
                bracket: bracket(
                    &pricing.resolve_for_input_tokens(tier.above_input_tokens.saturating_add(1)),
                ),
            })
            .collect(),
    }
}

/// Common settlement calculation for metering and evaluation. No live tariff
/// lookup or model-name reconstruction is permitted after admission.
pub fn settlement_charge_evidence(ctx: &SettlementContext) -> ChargeEvidence {
    let usage = Usage {
        prompt_tokens: ctx.prompt_tokens,
        completion_tokens: ctx.completion_tokens,
        reasoning_tokens: ctx.reasoning_tokens,
        cache_read_tokens: ctx.cache_read_tokens,
        cache_write_tokens: ctx.cache_write_tokens,
        web_search_count: ctx.web_search_count,
        origin: ctx.usage_origin,
        raw: ctx.raw_usage.clone().map(Box::new),
        availability: ctx.usage_availability.clone(),
    };
    let Some(target) = &ctx.target else {
        let reason = if usage.origin == UsageOrigin::Unknown {
            "usage_unavailable"
        } else {
            "target_protocol_unavailable"
        };
        return unavailable_charge_evidence(&usage, reason);
    };
    if target.api_protocol.operation() != ctx.operation
        || target.provider_name != ctx.provider_id
        || target.service_id != ctx.model_id
    {
        return unavailable_charge_evidence(&usage, "settlement_target_mismatch");
    }
    let key = PricingTargetKey::from_target(target);
    match ctx
        .get_events::<TargetTariffSnapshot>()
        .into_iter()
        .rev()
        .find(|snapshot| snapshot.target == key)
    {
        Some(snapshot) => snapshot
            .tariff
            .charge_evidence(&usage, PricingSource::Configured),
        None => unavailable_charge_evidence(&usage, "pricing_snapshot_unavailable"),
    }
}
