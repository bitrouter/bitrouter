//! Per-attempt estimates share settlement's frozen price table and calculator.
//! They are audit evidence only and never write or reconcile a metering row.

use bitrouter_sdk::language_model::native::NativeAttemptReport;
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostEstimator, NativeTokenCost, NativeTokenRates, validated_provider_usage,
};

use super::pricing::{MAX_TRUSTED_TOKENS, PricingSource, PricingTable, calculate_charge_evidence};

impl NativeCostEstimator for PricingTable {
    fn estimate(&self, report: &NativeAttemptReport) -> NativeTokenCost {
        match estimate(self, report) {
            Ok(cost) => cost,
            Err(reason) => NativeTokenCost::unknown(reason),
        }
    }
}

fn estimate(
    table: &PricingTable,
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
    let pricing = table
        .resolve(provider, model)
        .ok_or("pricing_unavailable")?;
    if pricing
        .resolve_for_input_tokens(usage.prompt_tokens)
        .is_unconfigured()
    {
        return Err("pricing_unavailable");
    }
    let evidence = calculate_charge_evidence(usage, &pricing, PricingSource::Configured);
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
    use super::super::pricing::{ContextTier, ModelPricing};
    use super::*;
    use bitrouter_sdk::language_model::native::NativeRoute;
    use bitrouter_sdk::language_model::types::{ApiProtocol, GenerateResult, Usage, UsageOrigin};
    use serde_json::json;

    fn report() -> NativeAttemptReport {
        NativeAttemptReport {
            request_id: "request".into(),
            attempt_index: 0,
            route: NativeRoute {
                provider: "route-provider".into(),
                model: "alias".into(),
                protocol: ApiProtocol::Responses,
                constraints: Default::default(),
                output_token_limit_supported: Some(true),
                input_count: None,
            },
            actual_provider: Some("execution-provider".into()),
            actual_model: Some("served-model".into()),
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
        table
    }

    fn pricing() -> ModelPricing {
        ModelPricing::cache_aware(Some(2.0), Some(0.5), Some(3.0), Some(4.0))
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
        let cost = table.estimate(&report);
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
        assert_eq!(evidence.pricing_version, pricing_version);
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
                matches!(table.estimate(&report), NativeTokenCost::Unknown { .. }),
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
                self::table(price).estimate(&report()),
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
            table(pricing()).estimate(&report),
            NativeTokenCost::unknown("usage_exceeds_trusted_bound")
        );
        assert_eq!(
            table(ModelPricing::cache_aware(
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(0.0)
            ))
            .estimate(&self::report())
            .estimated_micro_usd(),
            Some(0)
        );
        Ok(())
    }
}
