//! Request-owned protocol tariffs captured after final route mutations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::RouteHook;
use crate::gateway::pipeline::settlement::SettlementContext;
use crate::gateway::pipeline::stream::{
    PricingTargetKey, UsagePricing, UsagePricingBracket, UsagePricingSnapshot, UsagePricingTier,
};
use crate::gateway::pipeline::types::RoutingTarget;
use async_trait::async_trait;
use bitrouter_ai::event::PipelineEvent;
use bitrouter_ai::types::{ApiProtocol, Usage, UsageOrigin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::pricing::{
    ChargeEvidence, ModelPricing, PricingSource, PricingTable, calculate_charge_evidence,
    pricing_version, unavailable_charge_evidence,
};

/// Full tariff and applicability frozen for one effective target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenTariff {
    /// Exact decimal Registry v1 tariff frozen with its selected interface.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "bitrouter_registry::json::optional_document"
    )]
    pub registry_price: Option<registry_json::Value>,
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
        if let Some(price) = &self.registry_price {
            return self.registry_charge_evidence(price, usage, source);
        }
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
        if let Some(price) = &self.registry_price {
            return self.protocol != ApiProtocol::Decisions
                && self.unavailable_reason.is_none()
                && price["kind"] == "token"
                && price["currency"] == "USD"
                && bitrouter_registry::document::ReaderPolicy::default().supports(price)
                && [
                    "/input_tokens/no_cache",
                    "/input_tokens/cache_read",
                    "/input_tokens/cache_write",
                    "/output_tokens/text",
                    "/output_tokens/reasoning",
                ]
                .iter()
                .all(|p| {
                    price
                        .pointer(p)
                        .is_some_and(registry_json::Value::is_number)
                });
        }
        if self.protocol == ApiProtocol::Decisions || self.unavailable_reason.is_some() {
            return false;
        }
        self.pricing.as_ref().is_some_and(|pricing| {
            complete_rates(pricing)
                && pricing.context_tiers.iter().all(|tier| {
                    complete_rates(
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
/// One physical inference HTTP dispatch. More than one requires codec-proven
/// billed-attempt aggregation, which the current host deliberately reports as
/// unavailable rather than charging only the successful final attempt.
#[derive(Debug, Default, Serialize)]
pub(crate) struct RegistryPhysicalAttempts {
    #[serde(skip)]
    count: AtomicU64,
    #[serde(skip)]
    registry_dispatched: AtomicBool,
    #[serde(skip)]
    pricing_unverified: AtomicBool,
    require_known: bool,
}

fn request_pricing_unverified(
    prompt: &bitrouter_ai::types::Prompt,
    protocol: &ApiProtocol,
) -> bool {
    // Processing tier is a request condition, not a property of the hostname.
    // https://developers.openai.com/api/reference/resources/responses/methods/create
    crate::gateway::model::protocol::values::extra_value_for_protocol(
        &prompt.params,
        protocol,
        "service_tier",
    )
    .is_some_and(|tier| tier.as_str() != Some("default"))
}

pub(crate) fn admit_registry_request_pricing(
    target: &RoutingTarget,
    prompt: &bitrouter_ai::types::Prompt,
    ctx: &PipelineContext,
) -> crate::gateway::error::Result<()> {
    if target.registry.is_some()
        && request_pricing_unverified(prompt, &target.api_protocol)
        && let Some(attempts) = ctx.get_event::<RegistryPhysicalAttempts>()
    {
        attempts.pricing_unverified.store(true, Ordering::Relaxed);
        if attempts.require_known {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "known registry price is unavailable for this request processing tier",
            ));
        }
    }
    Ok(())
}
impl RegistryPhysicalAttempts {
    pub(crate) fn dispatched(&self, registry: bool) {
        if registry {
            self.registry_dispatched.store(true, Ordering::Relaxed);
        }
        let mut count = self.count.load(Ordering::Relaxed);
        while let Some(next) = count.checked_add(1) {
            match self.count.compare_exchange_weak(
                count,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => count = observed,
            }
        }
    }
    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}
impl PipelineEvent for RegistryPhysicalAttempts {
    fn event_name(&self) -> &'static str {
        "registry.physical_attempts"
    }
}
impl PipelineEvent for TargetTariffSnapshot {
    fn event_name(&self) -> &'static str {
        "pricing.tariff_snapshot"
    }
}

impl PricingTable {
    /// Freeze the configured rates and the effective endpoint's applicability.
    pub fn snapshot(&self, target: &RoutingTarget) -> TargetTariffSnapshot {
        if let Some(selected) = &target.registry {
            return registry_snapshot(target, selected);
        }
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
        let basis = if target.api_protocol == ApiProtocol::Decisions {
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
                registry_price: None,
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
    ) -> crate::gateway::error::Result<()> {
        if !ctx.has_event::<RegistryPhysicalAttempts>() {
            ctx.emit(RegistryPhysicalAttempts {
                require_known: self.require_known,
                ..Default::default()
            });
        }
        let snapshots = chain
            .iter()
            .map(|target| {
                let mut snapshot = self.pricing.snapshot(target);
                if target.registry.is_some()
                    && ctx
                        .generation_prompt()
                        .is_some_and(|p| request_pricing_unverified(p, &target.api_protocol))
                {
                    snapshot.tariff.unavailable_reason =
                        Some("registry_processing_tier_unverified".into());
                    snapshot
                        .tariff
                        .pricing_version
                        .push_str(":processing-tier-unverified");
                }
                snapshot
            })
            .collect::<Vec<_>>();
        if self.require_known
            && snapshots
                .iter()
                .any(|snapshot| !snapshot.tariff.guarantees_known_price())
        {
            return Err(crate::gateway::error::BitrouterError::bad_request(
                "known-price coverage is unavailable for the selected route",
            ));
        }
        for snapshot in snapshots {
            let pricing = if snapshot.tariff.unavailable_reason.is_none() {
                snapshot
                    .tariff
                    .registry_price
                    .as_ref()
                    .map(|price| UsagePricing {
                        registry_price: Some(price.clone()),
                        ..Default::default()
                    })
                    .or_else(|| snapshot.tariff.pricing.as_ref().map(stream_pricing))
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

    async fn revalidate_context(
        &self,
        chain: &[RoutingTarget],
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        let snapshots = ctx.get_events::<TargetTariffSnapshot>();
        for target in chain {
            let key = PricingTargetKey::from_target(target);
            let snapshot = snapshots
                .iter()
                .rev()
                .find(|snapshot| snapshot.target == key)
                .ok_or_else(|| {
                    crate::gateway::error::BitrouterError::bad_request(
                        "rebuilt context has no admitted tariff for the selected route",
                    )
                })?;
            if self.require_known
                && (!snapshot.tariff.guarantees_known_price()
                    || (target.registry.is_some()
                        && ctx
                            .generation_prompt()
                            .is_some_and(|p| request_pricing_unverified(p, &target.api_protocol))))
            {
                return Err(crate::gateway::error::BitrouterError::bad_request(
                    "known-price coverage is unavailable for the selected route",
                ));
            }
        }
        // Context removal cannot refresh prices or change the admitted target.
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
        registry_price: None,
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
    };
    let Some(target) = &ctx.target else {
        let reason = if usage.origin == UsageOrigin::Unknown {
            "usage_unavailable"
        } else {
            "target_protocol_unavailable"
        };
        return unavailable_charge_evidence(&usage, reason);
    };
    let attempts = ctx.get_event::<RegistryPhysicalAttempts>();
    if attempts.is_some_and(|a| a.pricing_unverified.load(Ordering::Relaxed)) {
        return unavailable_charge_evidence(&usage, "registry_processing_tier_unverified");
    }
    if (target.registry.is_some()
        || attempts.is_some_and(|a| a.registry_dispatched.load(Ordering::Relaxed)))
        && attempts.is_none_or(|attempts| attempts.count() != 1)
    {
        return unavailable_charge_evidence(&usage, "registry_attempt_completeness_unavailable");
    }
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
        Some(snapshot) => {
            let mut evidence = snapshot
                .tariff
                .charge_evidence(&usage, PricingSource::Configured);
            if let Some(exact) = &mut evidence.exact_registry_charge {
                exact.scope = "logical_request".into();
            }
            evidence
        }
        None => unavailable_charge_evidence(&usage, "pricing_snapshot_unavailable"),
    }
}

fn registry_snapshot(
    target: &RoutingTarget,
    selected: &crate::gateway::catalog::execution::SelectedInterface,
) -> TargetTariffSnapshot {
    let price = selected.interface.get("pricing").cloned();
    let profile = endpoint_profile(target.effective_api_base());
    let same_endpoint = selected.tariff_endpoint.as_deref().is_some_and(|base| {
        base.trim_end_matches('/') == target.effective_api_base().trim_end_matches('/')
    });
    let declared = price.as_ref().and_then(|p| p["endpoint_profile"].as_str());
    let reason = if crate::gateway::catalog::execution::validate_target(target).is_err() {
        Some("registry_target_mismatch")
    } else if selected.billing.as_deref() != Some("usage") {
        Some("registry_billing_unavailable")
    } else if price.is_none() {
        Some("pricing_not_found")
    } else if let Some(declared) = declared {
        if !matches!(declared, "openai_global" | "openai_us" | "openai_europe")
            || selected.verified_profile.as_deref() != Some(declared)
            || profile != declared
        {
            Some("endpoint_profile_unverified")
        } else {
            None
        }
    } else if !same_endpoint {
        Some("tariff_endpoint_changed")
    } else {
        None
    };
    let version = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(
            format!(
                "registry-tariff-v1|{}|{}|{:?}|{:?}|{:?}",
                target.api_protocol, profile, selected.verified_profile, price, reason
            )
            .as_bytes()
        ))
    );
    TargetTariffSnapshot {
        target: PricingTargetKey::from_target(target),
        tariff: FrozenTariff {
            registry_price: price,
            protocol: target.api_protocol.clone(),
            endpoint_profile: profile,
            tariff_profile: selected.verified_profile.clone(),
            pricing: None,
            pricing_version: version,
            billing_basis: "registry-exact-disjoint-usage-v1".into(),
            unavailable_reason: reason.map(str::to_owned),
        },
    }
}
impl FrozenTariff {
    fn registry_charge_evidence(
        &self,
        price: &registry_json::Value,
        usage: &Usage,
        source: PricingSource,
    ) -> ChargeEvidence {
        use bitrouter_registry::{
            document::ReaderPolicy,
            pricing::{TokenUsage, charge},
        };
        use num_rational::BigRational;
        use num_traits::ToPrimitive;
        let calculate = || -> Option<(i64, bitrouter_ai::types::NormalizedUsage,super::pricing::ExactRegistryCharge)> {
            if self.unavailable_reason.is_some()
                || !matches!(
                    usage.origin,
                    UsageOrigin::ProviderReported | UsageOrigin::AuthoritativeReceipt
                )
                || usage.web_search_count > 0
            {
                return None;
            }
            // This codec does not establish cache billing for native Decisions.
            if self.protocol == ApiProtocol::Decisions
                && (usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0)
            {
                return None;
            }
            let normalized = usage.normalized_buckets().ok()?;
            let tokens = TokenUsage {
                uncached_input: Some(normalized.uncached_input_tokens),
                cache_read: Some(normalized.cache_read_tokens),
                cache_write: Some(normalized.cache_write_tokens),
                non_reasoning_output: Some(normalized.output_tokens),
                reasoning_output: Some(normalized.reasoning_tokens),
            };
            // No meter is inferred from a response field or unit name. Until a
            // codec registers counters, metered tariffs remain unavailable.
            let amount = charge(
                price,
                &tokens,
                &Default::default(),
                self.tariff_profile.as_deref(),
                &ReaderPolicy::default(),
            )
            .ok()?;
            let exact=super::pricing::ExactRegistryCharge{currency:"USD".into(),numerator:amount.numer().to_string(),denominator:amount.denom().to_string(),scope:"attempt".into()};
            let micro = amount * BigRational::from_integer(1_000_000.into());
            let rounded = (micro + BigRational::new(1.into(), 2.into()))
                .to_integer()
                .to_i64()?;
            Some((rounded, normalized, exact))
        };
        let mut evidence = unavailable_charge_evidence(
            usage,
            self.unavailable_reason
                .as_deref()
                .unwrap_or("registry_price_or_usage_unavailable"),
        );
        if let Some((amount, normalized, exact)) = calculate() {
            evidence.exact_registry_charge = Some(exact);
            evidence.status = super::pricing::ChargeStatus::Computed;
            evidence.charge_micro_usd = Some(amount);
            evidence.normalized_usage = normalized;
            evidence.pricing_source = source;
            evidence.unknown_reason = None;
        }
        evidence.pricing_version.clone_from(&self.pricing_version);
        evidence.tariff_snapshot = Some(self.clone());
        evidence
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use registry_json as serde_json;
    use serde_json::json;
    fn target(price: &str, protocol: ApiProtocol) -> anyhow::Result<RoutingTarget> {
        let mut raw = crate::gateway::catalog::tests::document("priced");
        let value = bitrouter_registry::document::parse_json(price.as_bytes())?;
        raw["data"]["providers"][0]["models"][0]["interfaces"][0]["pricing"] =
            json!({"kind":"token","currency":"USD","input_tokens":{"no_cache":100}});
        raw["data"]["providers"][0]["models"][0]["interfaces"][1]["pricing"] = value;
        let data = crate::gateway::catalog::decode::from_slice(&serde_json::to_vec(&raw)?)?;
        let mut config = crate::config::Config::default();
        crate::providers::registry::apply::apply_registry(&mut config, &data);
        crate::config::routing_table::resolve_route_chain(
            &config,
            "org/model",
            &crate::gateway::pipeline::routing::RoutingPrefs {
                inbound_protocol: Some(protocol),
                ..Default::default()
            },
        )?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no selected tariff route"))
    }
    #[test]
    fn exact_decimal_is_kept_through_selected_wire_and_runtime_settlement() -> anyhow::Result<()> {
        let price = r#"{"kind":"token","currency":"USD","input_tokens":{"no_cache":0.4999999999999999999999999999}}"#;
        let usage = Usage {
            prompt_tokens: 1,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };
        let table = PricingTable::new();
        let chat = table
            .snapshot(&target(price, ApiProtocol::ChatCompletions)?)
            .tariff;
        let exact = chat
            .charge_evidence(&usage, PricingSource::Configured)
            .exact_registry_charge
            .ok_or_else(|| anyhow::anyhow!("exact amount lost"))?;
        assert_eq!(exact.scope, "attempt");
        assert_ne!(exact.numerator, "0");
        assert_eq!(exact.currency, "USD");
        assert_eq!(
            chat.charge_evidence(&usage, PricingSource::Configured)
                .charge_micro_usd,
            Some(0)
        );
        let messages = table
            .snapshot(&target(price, ApiProtocol::Messages)?)
            .tariff;
        assert_eq!(
            messages
                .charge_evidence(&usage, PricingSource::Configured)
                .charge_micro_usd,
            Some(100)
        );
        let serialized = ::serde_json::to_vec(&chat)?;
        let restarted: FrozenTariff = ::serde_json::from_slice(&serialized)?;
        assert_eq!(
            restarted
                .charge_evidence(&usage, PricingSource::Configured)
                .charge_micro_usd,
            Some(0)
        );
        let mut changed = target(price, ApiProtocol::ChatCompletions)?;
        changed.api_base = "https://another.example/v1".into();
        assert!(
            table
                .snapshot(&changed)
                .tariff
                .charge_evidence(&usage, PricingSource::Configured)
                .charge_micro_usd
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn logical_charge_requires_complete_physical_attempt_evidence() -> anyhow::Result<()> {
        let route = target(
            r#"{"kind":"token","currency":"USD","input_tokens":{"no_cache":1}}"#,
            ApiProtocol::ChatCompletions,
        )?;
        let prompt = bitrouter_ai::types::Prompt {
            model: "org/model".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut context =
            PipelineContext::new(crate::gateway::pipeline::types::PipelineRequest::new(
                "org/model",
                crate::gateway::caller::CallerContext::local(),
                prompt,
            ));
        context.route_chain = Some(vec![route.clone()]);
        CaptureTariffs::new(Arc::new(PricingTable::new()), false)
            .after_resolve(std::slice::from_ref(&route), &mut context)
            .await?;
        context.set_successful_target(route);
        let mut settled = context.settlement_context();
        settled.prompt_tokens = 1;
        settled.usage_origin = UsageOrigin::ProviderReported;
        assert!(
            settlement_charge_evidence(&settled)
                .exact_registry_charge
                .is_none()
        );
        settled
            .get_event::<RegistryPhysicalAttempts>()
            .ok_or_else(|| anyhow::anyhow!("attempt counter absent"))?
            .dispatched(true);
        assert_eq!(
            settlement_charge_evidence(&settled)
                .exact_registry_charge
                .map(|v| v.scope),
            Some("logical_request".into())
        );
        settled
            .get_event::<RegistryPhysicalAttempts>()
            .ok_or_else(|| anyhow::anyhow!("attempt counter absent"))?
            .dispatched(true);
        let evidence = settlement_charge_evidence(&settled);
        assert!(evidence.charge_micro_usd.is_none());
        assert_eq!(
            evidence.unknown_reason.as_deref(),
            Some("registry_attempt_completeness_unavailable")
        );
        // A manual final winner cannot erase a preceding registry attempt.
        settled
            .target
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("target absent"))?
            .registry = None;
        assert_eq!(
            settlement_charge_evidence(&settled)
                .unknown_reason
                .as_deref(),
            Some("registry_attempt_completeness_unavailable")
        );
        Ok(())
    }

    #[tokio::test]
    async fn nonstandard_processing_tiers_and_unknown_profiles_cannot_claim_known_price()
    -> anyhow::Result<()> {
        let price = r#"{"kind":"token","currency":"USD","input_tokens":{"no_cache":1,"cache_read":1,"cache_write":1},"output_tokens":{"text":1,"reasoning":1}}"#;
        let mut route = target(price, ApiProtocol::ChatCompletions)?;
        let pricing = Arc::new(PricingTable::new());
        assert!(pricing.snapshot(&route).tariff.guarantees_known_price());
        let mut prompt: bitrouter_ai::types::Prompt = serde_json::from_value(
            json!({"model":"org/model","messages":[],"tools":[],"params":{"extra":{"service_tier":"priority"}},"stream":false}),
        )?;
        let mut context =
            PipelineContext::new(crate::gateway::pipeline::types::PipelineRequest::new(
                "org/model",
                crate::gateway::caller::CallerContext::local(),
                prompt.clone(),
            ));
        assert!(
            CaptureTariffs::new(pricing.clone(), true)
                .after_resolve(std::slice::from_ref(&route), &mut context)
                .await
                .is_err()
        );
        CaptureTariffs::new(pricing.clone(), false)
            .after_resolve(std::slice::from_ref(&route), &mut context)
            .await?;
        assert_eq!(
            context
                .get_events::<TargetTariffSnapshot>()
                .last()
                .and_then(|s| s.tariff.unavailable_reason.as_deref()),
            Some("registry_processing_tier_unverified")
        );
        prompt.params.extra.clear();
        let mut ordinary =
            PipelineContext::new(crate::gateway::pipeline::types::PipelineRequest::new(
                "org/model",
                crate::gateway::caller::CallerContext::local(),
                prompt.clone(),
            ));
        CaptureTariffs::new(pricing.clone(), true)
            .after_resolve(std::slice::from_ref(&route), &mut ordinary)
            .await?;
        prompt
            .params
            .extra
            .insert("service_tier".into(), ::serde_json::json!("flex"));
        assert!(admit_registry_request_pricing(&route, &prompt, &ordinary).is_err());
        let selected = Arc::make_mut(
            route
                .registry
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no metadata"))?,
        );
        let profile = endpoint_profile(
            selected
                .tariff_endpoint
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("no endpoint"))?,
        );
        selected.verified_profile = Some(profile.clone());
        selected.interface["pricing"]["endpoint_profile"] = json!(profile);
        assert!(!pricing.snapshot(&route).tariff.guarantees_known_price());
        Ok(())
    }
    #[test]
    fn unsupported_meters_features_and_missing_rates_are_never_free() -> anyhow::Result<()> {
        let usage = Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };
        for price in [
            r#"{"kind":"metered","currency":"USD","requires":["pricing.metered.v1"],"meters":[{"id":"answered_question","unit":"decision","quantity":1000,"amount":"0.20"}]}"#,
            r#"{"kind":"token","currency":"USD","requires":["future.pricing"],"input_tokens":{"no_cache":0},"output_tokens":{"text":0}}"#,
            r#"{"kind":"token","currency":"USD","input_tokens":{"no_cache":0}}"#,
        ] {
            let tariff = PricingTable::new()
                .snapshot(&target(price, ApiProtocol::ChatCompletions)?)
                .tariff;
            assert!(
                tariff
                    .charge_evidence(&usage, PricingSource::Configured)
                    .charge_micro_usd
                    .is_none()
            );
        }
        Ok(())
    }
}
