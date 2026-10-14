//! Per-attempt estimates share settlement's frozen price table and calculator.
//! They are audit evidence only and never write or reconcile a metering row.

use bitrouter_sdk::language_model::native::NativeAttemptReport;
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostEstimator, NativeTokenCost, NativeTokenRates, NativeTokenReservation,
    validated_provider_usage,
};

use super::pricing::{MAX_TRUSTED_TOKENS, PricingSource, PricingTable};

impl NativeCostEstimator for PricingTable {
    fn reservation_for_attempt(
        &self,
        route: &bitrouter_sdk::language_model::native::NativeRoute,
        prompt: &bitrouter_ai::types::Prompt,
        ctx: &bitrouter_sdk::language_model::context::PipelineContext,
        target: &bitrouter_sdk::language_model::types::RoutingTarget,
    ) -> Option<NativeTokenReservation> {
        use bitrouter_sdk::language_model::native::{NativeInputCount, NativeProtocolValidation};
        if route.provider != target.provider_name
            || route.model != target.service_id
            || route.protocol != target.api_protocol
            || route.protocol_validation != NativeProtocolValidation::Compatible
        {
            return None;
        }
        let key = bitrouter_sdk::language_model::stream::PricingTargetKey::from_target(target);
        let snapshot = ctx
            .get_events::<super::tariff::TargetTariffSnapshot>()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.target == key)?;
        if !snapshot.tariff.guarantees_known_price() {
            return None;
        }
        let input = match &route.input_count {
            Some(NativeInputCount::Counted { input_tokens, .. }) => *input_tokens,
            Some(NativeInputCount::Unavailable { .. }) => return None,
            None => route.constraints.token_limits.max_input_tokens?,
        };
        let output = u64::from(prompt.params.max_tokens?);
        if input == 0
            || input > MAX_TRUSTED_TOKENS
            || output == 0
            || output > MAX_TRUSTED_TOKENS
            || !(route.output_token_limit_supported == Some(true)
                || route
                    .constraints
                    .token_limits
                    .output_reservation_covers_model(prompt.params.max_tokens?))
        {
            return None;
        }
        let pricing = snapshot.tariff.pricing.as_ref()?;
        let mut brackets = vec![pricing.clone()];
        for tier in &pricing.context_tiers {
            if tier.above_input_tokens < input {
                brackets.push(
                    pricing.resolve_for_input_tokens(tier.above_input_tokens.checked_add(1)?),
                );
            }
        }
        let mut input_rate = 0.0_f64;
        let mut output_rate = 0.0_f64;
        for bracket in brackets {
            for rate in [
                bracket.input_micro_usd_per_token?,
                bracket.cache_read_micro_usd_per_token?,
                bracket.cache_write_micro_usd_per_token?,
            ] {
                if !rate.is_finite() || rate < 0.0 {
                    return None;
                }
                input_rate = input_rate.max(rate);
            }
            let rate = bracket.output_micro_usd_per_token?;
            if !rate.is_finite() || rate < 0.0 {
                return None;
            }
            output_rate = output_rate.max(rate);
        }
        let amount = (input as f64 * input_rate + output as f64 * output_rate).ceil();
        // At most five nonnegative bucket products/additions enter settlement.
        // Keep their magnitude below 2^52 and cover floating-point rounding;
        // authoritative zero rates still have a zero token reservation.
        if !amount.is_finite() || amount < 0.0 || amount > (1_u64 << 52) as f64 {
            return None;
        }
        Some(NativeTokenReservation {
            micro_usd: (amount as u64).checked_add(if input_rate == 0.0 && output_rate == 0.0 {
                0
            } else {
                8
            })?,
            pricing_version: snapshot.tariff.pricing_version.clone(),
            provider: target.provider_name.clone(),
            model: target.service_id.clone(),
            input_tokens_bound: input,
            output_tokens_bound: output,
            evidence: serde_json::json!({"tariff":snapshot.tariff,"input_count":route.input_count,"limits":route.constraints.token_limits,"output_token_limit_supported":route.output_token_limit_supported}),
        })
    }

    fn estimate(&self, _report: &NativeAttemptReport) -> NativeTokenCost {
        NativeTokenCost::unknown("pricing_snapshot_unavailable")
    }

    fn estimate_for_attempt(
        &self,
        report: &NativeAttemptReport,
        ctx: &bitrouter_sdk::language_model::context::PipelineContext,
        target: &bitrouter_sdk::language_model::types::RoutingTarget,
    ) -> NativeTokenCost {
        if report.actual_provider.as_deref() != Some(target.provider_name.as_str())
            || report.actual_model.as_deref() != Some(target.service_id.as_str())
            || report.route.protocol != target.api_protocol
        {
            return NativeTokenCost::unknown("execution_target_mismatch");
        }
        let key = bitrouter_sdk::language_model::stream::PricingTargetKey::from_target(target);
        let snapshot = ctx
            .get_events::<super::tariff::TargetTariffSnapshot>()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.target == key);
        match snapshot {
            Some(snapshot) => {
                estimate(&snapshot.tariff, report).unwrap_or_else(NativeTokenCost::unknown)
            }
            None => NativeTokenCost::unknown("pricing_snapshot_unavailable"),
        }
    }
}

fn estimate(
    tariff: &super::tariff::FrozenTariff,
    report: &NativeAttemptReport,
) -> Result<NativeTokenCost, &'static str> {
    let usage = report
        .result
        .as_ref()
        .and_then(|result| result.usage.as_ref())
        .ok_or("usage_unavailable")?;
    let normalized = validated_provider_usage(&report.route.protocol, usage)?;
    // The legacy settlement calculator clamps extreme counts and saturates its
    // signed integer result. Such evidence must remain unknown in core receipts.
    if [
        normalized.uncached_input_tokens,
        normalized.cache_read_tokens,
        normalized.cache_write_tokens,
        normalized.output_tokens,
        normalized.reasoning_tokens,
    ]
    .into_iter()
    .any(|count| count > MAX_TRUSTED_TOKENS)
    {
        return Err("usage_exceeds_trusted_bound");
    }
    let provider = report
        .actual_provider
        .as_deref()
        .ok_or("execution_identity_unavailable")?;
    let model = report
        .actual_model
        .as_deref()
        .ok_or("execution_identity_unavailable")?;
    if let Some(reason) = tariff.unavailable_reason.as_deref() {
        return Ok(NativeTokenCost::unknown(reason));
    }
    let evidence = tariff.charge_evidence(usage, PricingSource::Configured);
    let effective = evidence.effective_rates;
    if [
        effective.uncached_input_micro_usd_per_token,
        effective.cache_read_micro_usd_per_token,
        effective.cache_write_micro_usd_per_token,
        effective.output_micro_usd_per_token,
    ]
    .into_iter()
    .flatten()
    .any(|rate| !rate.is_finite() || rate < 0.0)
    {
        return Err("invalid_configured_rate");
    }
    let Some(amount) = evidence.charge_micro_usd else {
        return Ok(NativeTokenCost::unknown(
            evidence
                .unknown_reason
                .unwrap_or_else(|| "charge_unavailable".into()),
        ));
    };
    if amount == i64::MAX {
        return Err("charge_overflow");
    }
    let micro_usd = u64::try_from(amount).map_err(|_| "invalid_charge_total")?;
    Ok(NativeTokenCost::ConfiguredEstimate {
        micro_usd,
        usage_origin: usage.origin,
        normalized_usage: evidence.normalized_usage,
        rates: NativeTokenRates {
            uncached_input: effective.uncached_input_micro_usd_per_token,
            cache_read: effective.cache_read_micro_usd_per_token,
            cache_write: effective.cache_write_micro_usd_per_token,
            output: effective.output_micro_usd_per_token,
        },
        pricing_version: evidence.pricing_version,
        pricing_provider: provider.into(),
        pricing_model: model.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::pricing::calculate_charge_evidence;
    use super::super::pricing::{ContextTier, ModelPricing};
    use super::*;
    use bitrouter_ai::types::{ApiProtocol, GenerateResult, Usage, UsageOrigin};
    use bitrouter_sdk::language_model::native::NativeRoute;
    use serde_json::json;

    fn target() -> bitrouter_sdk::language_model::types::RoutingTarget {
        bitrouter_sdk::language_model::types::RoutingTarget {
            provider_name: "execution-provider".into(),
            service_id: "served-model".into(),
            api_base: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: bitrouter_ai::types::AuthScheme::Bearer,
            headers: Vec::new(),
        }
    }

    fn estimate_fixture(table: &PricingTable, report: &NativeAttemptReport) -> NativeTokenCost {
        let target = target();
        let ctx = fixture_context(table, &target);
        table.estimate_for_attempt(report, &ctx, &target)
    }

    fn fixture_context(
        table: &PricingTable,
        target: &bitrouter_sdk::language_model::types::RoutingTarget,
    ) -> bitrouter_sdk::language_model::context::PipelineContext {
        let prompt = bitrouter_ai::types::Prompt {
            model: "served-model".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut ctx = bitrouter_sdk::language_model::context::PipelineContext::new(
            bitrouter_sdk::language_model::types::PipelineRequest::new(
                "served-model",
                bitrouter_sdk::caller::CallerContext::local(),
                prompt,
            ),
        );
        ctx.emit(table.snapshot(target));
        ctx
    }

    fn report() -> NativeAttemptReport {
        NativeAttemptReport {
            private_context: Default::default(),
            continuation: Default::default(),
            request_id: "request".into(),
            attempt_index: 0,
            route: NativeRoute {
                provider: "route-provider".into(),
                model: "alias".into(),
                protocol: ApiProtocol::Responses,
                constraints: Default::default(),
                output_token_limit_supported: Some(true),
                input_count: None,
                protocol_validation: Default::default(),
                continuation: Default::default(),
            },
            actual_provider: Some("execution-provider".into()),
            actual_model: Some("served-model".into()),
            output_rejection: None,
            report_rejection: None,
            result: Some(GenerateResult {
                content: Vec::new(),
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 5,
                    cache_read_tokens: 30,
                    cache_write_tokens: 10,
                    reasoning_tokens: 2,
                    origin: UsageOrigin::ProviderReported,
                    raw: Some(Box::new(
                        json!({"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":10},"output_tokens_details":{"reasoning_tokens":2}}),
                    )),
                    ..Default::default()
                }),
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            }),
            error: None,
            elapsed_ms: 1,
            token_cost: Default::default(),
            cache: Default::default(),
        }
    }

    fn table(pricing: ModelPricing) -> PricingTable {
        let mut table = PricingTable::new();
        table.insert("execution-provider", "served-model", pricing);
        table.configure_endpoint("execution-provider", None, "https://api.openai.com/v1");
        table
    }

    #[test]
    fn token_reservation_uses_frozen_endpoint_and_all_relevant_price_brackets() -> anyhow::Result<()>
    {
        let mut pricing = ModelPricing::cache_aware(Some(1.0), Some(2.0), Some(4.0), Some(3.0));
        pricing.context_tiers.push(ContextTier {
            above_input_tokens: 50,
            input_micro_usd_per_token: Some(5.0),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: Some(9.0),
            output_micro_usd_per_token: Some(7.0),
        });
        let table = table(pricing);
        let target = target();
        let ctx = fixture_context(&table, &target);
        let mut prompt = ctx.require_generation_prompt()?.clone();
        prompt.params.max_tokens = Some(10);
        let mut route = report().route;
        route.provider.clone_from(&target.provider_name);
        route.model.clone_from(&target.service_id);
        route.protocol_validation =
            bitrouter_sdk::language_model::native::NativeProtocolValidation::Compatible;
        route.constraints.token_limits.max_input_tokens = Some(100);
        let quote = table
            .reservation_for_attempt(&route, &prompt, &ctx, &target)
            .ok_or_else(|| anyhow::anyhow!("missing conservative reservation"))?;
        assert!(quote.micro_usd >= 970);
        assert_eq!(quote.input_tokens_bound, 100);
        assert_eq!(quote.output_tokens_bound, 10);
        assert_eq!(
            quote.pricing_version,
            table.snapshot(&target).tariff.pricing_version
        );
        let mut changed_endpoint = target.clone();
        changed_endpoint.api_base = "https://example.invalid/v1".into();
        assert!(
            table
                .reservation_for_attempt(&route, &prompt, &ctx, &changed_endpoint)
                .is_none()
        );
        route.output_token_limit_supported = None;
        assert!(
            table
                .reservation_for_attempt(&route, &prompt, &ctx, &target)
                .is_none()
        );
        Ok(())
    }

    fn pricing() -> ModelPricing {
        ModelPricing::cache_aware(Some(2.0), Some(0.5), Some(3.0), Some(4.0))
    }

    #[tokio::test]
    async fn rebuilt_context_reuses_frozen_tariffs_and_rejects_new_targets()
    -> bitrouter_sdk::Result<()> {
        use super::super::tariff::{CaptureTariffs, TargetTariffSnapshot};
        use bitrouter_sdk::language_model::hooks::RouteHook;
        use std::sync::Arc;

        let pricing = table(pricing());
        let route = target();
        let ctx = fixture_context(&pricing, &route);
        let before = ctx.get_events::<TargetTariffSnapshot>()[0].clone();
        // The live table is deliberately empty: revalidation must use the
        // admitted evidence rather than price the rebuilt prompt again.
        let hook = CaptureTariffs::new(Arc::new(PricingTable::new()), true);
        hook.revalidate_context(std::slice::from_ref(&route), &ctx)
            .await?;
        assert_eq!(
            ctx.get_events::<TargetTariffSnapshot>()[0].target,
            before.target
        );
        assert_eq!(
            ctx.get_events::<TargetTariffSnapshot>()[0].tariff,
            before.tariff
        );
        let mut changed = route;
        changed.api_base = "https://changed.invalid/v1".into();
        assert!(hook.revalidate_context(&[changed], &ctx).await.is_err());
        Ok(())
    }

    #[test]
    fn native_estimate_uses_served_identity_and_settlement_price_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = report();
        let mut price = pricing();
        price.context_tiers.push(ContextTier {
            above_input_tokens: 50,
            input_micro_usd_per_token: Some(3.0),
            ..Default::default()
        });
        let mut table = table(price.clone());
        table.insert(
            "route-provider",
            "served-model",
            ModelPricing::new(999.0, 999.0),
        );
        let cost = estimate_fixture(&table, &report);
        let NativeTokenCost::ConfiguredEstimate {
            micro_usd,
            pricing_version,
            rates,
            normalized_usage,
            pricing_provider,
            pricing_model,
            ..
        } = cost
        else {
            return Err("missing configured estimate".into());
        };
        assert_eq!(micro_usd, 245);
        assert_eq!(pricing_provider, "execution-provider");
        assert_eq!(pricing_model, "served-model");
        let usage = report
            .result
            .as_ref()
            .and_then(|result| result.usage.as_ref())
            .ok_or("missing usage")?;
        let evidence = calculate_charge_evidence(usage, &price, PricingSource::Configured);
        assert_eq!(evidence.charge_micro_usd, Some(micro_usd as i64));
        assert_eq!(evidence.normalized_usage, normalized_usage);
        assert_eq!(
            table.snapshot(&target()).tariff.pricing_version,
            pricing_version
        );
        assert_eq!(rates.uncached_input, Some(3.0));
        assert_eq!(rates.cache_read, Some(0.5));
        Ok(())
    }

    #[test]
    fn native_estimate_missing_or_invalid_evidence_stays_unknown()
    -> Result<(), Box<dyn std::error::Error>> {
        let table = table(pricing());
        for case in 0..8 {
            let mut report = report();
            match case {
                0 => report.result = None,
                1 => report.actual_provider = None,
                2 => report.actual_model = Some("unpriced".into()),
                _ => {
                    let result = report.result.as_mut().ok_or("missing result")?;
                    if case == 3 {
                        result.usage = None;
                    } else {
                        let usage = result.usage.as_mut().ok_or("missing usage")?;
                        match case {
                            4 => usage.origin = UsageOrigin::Estimated,
                            5 => usage.raw = None,
                            6 => usage.raw = Some(Box::new(json!({"input_tokens":100}))),
                            _ => {
                                usage.raw =
                                    Some(Box::new(json!({"input_tokens":99,"output_tokens":5})))
                            }
                        }
                    }
                }
            }
            assert!(
                matches!(
                    estimate_fixture(&table, &report),
                    NativeTokenCost::Unknown { .. }
                ),
                "case {case}"
            );
        }
        for price in [
            ModelPricing::default(),
            ModelPricing::new(2.0, 4.0),
            ModelPricing::cache_aware(Some(f64::INFINITY), Some(0.5), Some(3.0), Some(4.0)),
            ModelPricing::cache_aware(Some(2.0), Some(-1.0), Some(3.0), Some(4.0)),
            ModelPricing::cache_aware(Some(1e30), Some(0.5), Some(3.0), Some(4.0)),
        ] {
            assert!(matches!(
                estimate_fixture(&self::table(price), &report()),
                NativeTokenCost::Unknown { .. }
            ));
        }
        Ok(())
    }

    #[test]
    fn native_estimate_rejects_clamping_but_preserves_configured_free_usage()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut report = report();
        let usage = report
            .result
            .as_mut()
            .and_then(|result| result.usage.as_mut())
            .ok_or("missing usage")?;
        usage.prompt_tokens = MAX_TRUSTED_TOKENS + 100;
        if let Some(raw) = usage.raw.as_mut() {
            raw["input_tokens"] = usage.prompt_tokens.into();
        }
        assert_eq!(
            estimate_fixture(&table(pricing()), &report),
            NativeTokenCost::unknown("usage_exceeds_trusted_bound")
        );
        assert_eq!(
            estimate_fixture(
                &table(ModelPricing::cache_aware(
                    Some(0.0),
                    Some(0.0),
                    Some(0.0),
                    Some(0.0)
                )),
                &self::report()
            )
            .estimated_micro_usd(),
            Some(0)
        );
        Ok(())
    }
    #[test]
    fn native_estimate_requires_the_admitted_protocol_and_endpoint_snapshot() {
        let mut table = table(pricing());
        let route = target();
        let ctx = fixture_context(&table, &route);
        let report = report();
        let frozen = table.estimate_for_attempt(&report, &ctx, &route);
        assert!(frozen.estimated_micro_usd().is_some());
        assert_eq!(
            table.estimate(&report),
            NativeTokenCost::unknown("pricing_snapshot_unavailable")
        );
        table.insert_for_protocol(
            "execution-provider",
            "served-model",
            ApiProtocol::Responses,
            ModelPricing::cache_aware(Some(20.0), Some(5.0), Some(30.0), Some(40.0)),
        );
        assert_eq!(table.estimate_for_attempt(&report, &ctx, &route), frozen);
        assert_ne!(estimate_fixture(&table, &report), frozen);
        let mut changed = route.clone();
        changed.api_base_override = Some("https://changed.example/v1".into());
        assert_eq!(
            table.estimate_for_attempt(&report, &ctx, &changed),
            NativeTokenCost::unknown("pricing_snapshot_unavailable")
        );
        assert_eq!(
            table.estimate_for_attempt(&report, &fixture_context(&table, &changed), &changed),
            NativeTokenCost::unknown("endpoint_profile_mismatch")
        );
        changed.api_protocol = ApiProtocol::ChatCompletions;
        assert_eq!(
            table.estimate_for_attempt(&report, &ctx, &changed),
            NativeTokenCost::unknown("execution_target_mismatch")
        );
    }
}
