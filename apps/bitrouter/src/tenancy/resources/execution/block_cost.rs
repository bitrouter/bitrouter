//! Strict token-price evidence for owned Block outcomes.

use bitrouter_ai::types::Usage;
use num_rational::BigRational;
use num_traits::{Signed, ToPrimitive};

use crate::gateway::pipeline::stream::UsagePricing;

pub(super) fn known(pricing: &UsagePricing, usage: &Usage) -> Option<BigRational> {
    // Compiler-v1 binds only explicit tariffs, not registry interface authority.
    if pricing.registry_price.is_some() {
        return None;
    }
    // Match the nominal selector's exclusive threshold. The retained config
    // projection has already inherited each omitted tier rate from the base.
    let bracket = pricing
        .context_tiers
        .iter()
        .filter(|tier| usage.prompt_tokens > tier.above_input_tokens)
        .max_by_key(|tier| tier.above_input_tokens)
        .map_or(&pricing.base, |tier| &tier.bracket);
    for (count, rate) in [
        (
            usage.cache_read_tokens,
            bracket.cache_read_micro_usd_per_token,
        ),
        (
            usage.cache_write_tokens,
            bracket.cache_write_micro_usd_per_token,
        ),
    ] {
        if count != 0 && !rate.is_some_and(|rate| rate.is_finite() && rate >= 0.0) {
            return None;
        }
    }
    // Nominal stream comparison may substitute input rates for missing cache
    // tariffs. That fallback cannot establish Block cost evidence for a used
    // cache bucket. Keep its arithmetic and numeric rejection rules unchanged.
    pricing.nominal_cost(usage)
}

/// Round only after all receipt-compatible candidates agree on the exact cost.
pub(super) fn receipt_cost(cost: &BigRational) -> Option<u64> {
    // Keep the existing exactly representable JSON integer limit. Compare
    // before rounding so conversion cannot hide a fraction above the limit.
    let maximum = BigRational::from_integer(9_007_199_254_740_991_u64.into());
    if cost.is_negative() || cost > &maximum {
        return None;
    }
    cost.ceil().to_integer().to_u64()
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, ensure};
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        config::{
            Config,
            routing_table::{resolve_clean_route_chain, retained_usage_pricing},
        },
        gateway::pipeline::{routing::RoutingPrefs, stream::UsagePricingBracket},
    };

    fn pricing(read: Option<f64>, write: Option<f64>) -> UsagePricing {
        UsagePricing {
            registry_price: None,
            base: UsagePricingBracket {
                input_micro_usd_per_token: Some(2.0),
                cache_read_micro_usd_per_token: read,
                cache_write_micro_usd_per_token: write,
                output_micro_usd_per_token: Some(3.0),
                ..Default::default()
            },
            context_tiers: Vec::new(),
        }
    }

    fn exact(value: f64) -> Option<BigRational> {
        BigRational::from_float(value)
    }

    fn usage(read: u64, write: u64) -> Usage {
        Usage {
            prompt_tokens: 4,
            completion_tokens: 2,
            cache_read_tokens: read,
            cache_write_tokens: write,
            ..Default::default()
        }
    }

    fn projected(tariff: Value) -> Result<UsagePricing> {
        let config: Config = serde_json::from_value(json!({
            "providers": {"fixture": {
                "api_base": "https://example.invalid/v1",
                "models": [{
                    "id": "model", "api_protocol": "chat_completions", "pricing": tariff
                }]
            }}
        }))?;
        let targets =
            resolve_clean_route_chain(&config, "fixture:model", &RoutingPrefs::default())?;
        ensure!(targets.len() == 1, "one pure tariff target");
        retained_usage_pricing(&config, targets.first().context("tariff target")?)
            .context("projected tariff")
    }

    #[test]
    fn block_receipt_match_uses_opaque_slots_and_rejects_foreign_identity() -> Result<()> {
        use crate::tenancy::receipts::journal::project_target;
        use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
        use bitrouter_ai::tenancy::configuration::ResourceRef;

        let config: Config = serde_json::from_value(json!({
            "providers": {"fixture": {"api_base": "https://example.invalid/v1",
                "models": [{"id": "model", "api_protocol": "chat_completions"}]}}
        }))?;
        let target = resolve_clean_route_chain(&config, "fixture:model", &RoutingPrefs::default())?
            .into_iter()
            .next()
            .context("actual retained candidate")?;
        let route = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "route".try_into()?,
            revision_id: "r1".try_into()?,
        };
        let credential = ResourceRef {
            resource_kind: ResourceKind::Credential,
            resource_id: "slot".try_into()?,
            ..route.clone()
        };
        let candidate = super::super::Candidate {
            routing: Default::default(),
            target,
            projection: Vec::new(),
            credential,
        };
        let installation = "installation".try_into()?;
        let expected = project_target(
            &installation,
            &route,
            &candidate.credential,
            &candidate.target.provider_name,
            &candidate.target.service_id,
            candidate.target.account_label.as_deref(),
        )?;
        ensure!(
            expected.account_id.as_str().starts_with("acct1-")
                && expected.credential_id.as_str().starts_with("cred1-")
        );
        ensure!(candidate.receipt_target(&installation, &route, &expected));
        for field in [
            "provider_id",
            "model_id",
            "account_id",
            "credential_id",
            "credential_version",
        ] {
            let mut value = serde_json::to_value(&expected)?;
            value[field] = json!("changed");
            let changed = serde_json::from_value(value)?;
            ensure!(
                !candidate.receipt_target(&installation, &route, &changed),
                "accepted changed {field}"
            );
        }
        let mut raw = expected.clone();
        raw.account_id = candidate
            .target
            .account_label
            .as_deref()
            .unwrap_or("default")
            .try_into()?;
        raw.credential_id = candidate.credential.resource_id.as_str().try_into()?;
        ensure!(!candidate.receipt_target(&installation, &route, &raw));
        ensure!(!candidate.receipt_target(&"other-installation".try_into()?, &route, &expected));
        let mut other = route.clone();
        other.owner_id = "other-tenant".try_into()?;
        ensure!(!candidate.receipt_target(&installation, &other, &expected));
        other = route.clone();
        other.owner_kind = OwnerKind::OperatorPool;
        ensure!(!candidate.receipt_target(&installation, &other, &expected));
        other = route.clone();
        other.resource_id = "other-route".try_into()?;
        ensure!(!candidate.receipt_target(&installation, &other, &expected));
        Ok(())
    }

    #[test]
    fn missing_cache_tariffs_cannot_become_known_block_cost() {
        let tariff = pricing(None, None);
        for used in [usage(2, 0), usage(0, 2), usage(1, 1)] {
            assert_eq!(known(&tariff, &used), None);
            // Legacy stream selection deliberately retains its nominal fallback.
            assert_eq!(tariff.nominal_cost(&used), exact(14.0));
        }
        assert_eq!(known(&tariff, &usage(0, 0)), exact(14.0));
        assert_eq!(known(&pricing(Some(0.5), None), &usage(1, 0)), exact(12.5));
        assert_eq!(known(&pricing(None, Some(4.0)), &usage(0, 1)), exact(16.0));
    }

    #[test]
    fn explicit_zero_and_positive_cache_tariffs_are_exact_evidence() {
        for used in [usage(2, 0), usage(0, 2), usage(1, 1)] {
            assert_eq!(known(&pricing(Some(0.0), Some(0.0)), &used), exact(10.0));
        }
        assert_eq!(
            known(&pricing(Some(0.5), Some(4.0)), &usage(1, 1)),
            exact(14.5)
        );
    }

    #[test]
    fn invalid_cache_rates_are_not_replaced_with_input_rates() {
        for rate in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(known(&pricing(Some(rate), Some(0.0)), &usage(1, 0)), None);
            assert_eq!(known(&pricing(Some(0.0), Some(rate)), &usage(0, 1)), None);
            // Preserve the nominal helper's rejection of malformed unused rates.
            assert_eq!(known(&pricing(Some(rate), Some(0.0)), &usage(0, 0)), None);
        }
    }

    #[test]
    fn retained_tiers_inherit_explicit_cache_rates_with_exclusive_thresholds() -> Result<()> {
        let tariff = projected(json!({
            "input_micro_usd_per_token": 2.0,
            "output_micro_usd_per_token": 3.0,
            "cache_read_micro_usd_per_token": 1.0,
            "cache_write_micro_usd_per_token": 5.0,
            "context_tiers": [
                {"above_input_tokens": 8, "input_micro_usd_per_token": 8.0,
                 "output_micro_usd_per_token": 9.0},
                {"above_input_tokens": 4, "input_micro_usd_per_token": 4.0,
                 "output_micro_usd_per_token": 6.0}
            ]
        }))?;
        for (prompt_tokens, expected) in [(4, 16.0), (5, 30.0), (8, 42.0), (9, 80.0)] {
            let used = Usage {
                prompt_tokens,
                ..usage(1, 1)
            };
            assert_eq!(known(&tariff, &used), exact(expected));
        }
        Ok(())
    }

    #[test]
    fn retained_tiers_do_not_inherit_cache_tariffs_from_previous_tiers() -> Result<()> {
        let tariff = projected(json!({
            "input_micro_usd_per_token": 2.0,
            "output_micro_usd_per_token": 3.0,
            "cache_write_micro_usd_per_token": 3.0,
            "context_tiers": [
                {"above_input_tokens": 4, "cache_read_micro_usd_per_token": 0.0,
                 "cache_write_micro_usd_per_token": 0.0},
                {"above_input_tokens": 8},
                {"above_input_tokens": 12, "cache_read_micro_usd_per_token": 2.0}
            ]
        }))?;
        for (prompt_tokens, expected) in [
            (4, None),
            (5, Some(12.0)),
            (8, Some(18.0)),
            (9, None),
            (13, Some(33.0)),
        ] {
            let used = Usage {
                prompt_tokens,
                ..usage(1, 1)
            };
            assert_eq!(known(&tariff, &used), expected.and_then(exact));
        }
        Ok(())
    }

    #[test]
    fn nominal_numeric_guards_and_reasoning_semantics_remain_in_force() {
        let tariff = pricing(Some(0.0), Some(0.0));
        for invalid in [
            usage(5, 0),
            usage(u64::MAX, 1),
            Usage {
                reasoning_tokens: 3,
                ..usage(0, 0)
            },
            Usage {
                web_search_count: 1,
                ..usage(0, 0)
            },
        ] {
            assert_eq!(known(&tariff, &invalid), None);
        }
        for rate in [
            None,
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::MAX),
        ] {
            let mut invalid = tariff.clone();
            invalid.base.input_micro_usd_per_token = rate;
            assert_eq!(known(&invalid, &usage(0, 0)), None);
            let mut invalid = tariff.clone();
            invalid.base.output_micro_usd_per_token = rate;
            assert_eq!(known(&invalid, &usage(0, 0)), None);
        }
        let reasoning = Usage {
            reasoning_tokens: 1,
            ..usage(0, 0)
        };
        assert_eq!(known(&tariff, &reasoning), exact(14.0));
    }

    #[test]
    fn registry_tariffs_cannot_supply_owned_block_evidence() {
        let mut tariff = pricing(Some(0.0), Some(0.0));
        tariff.registry_price = Some(registry_json::json!({"currency": "USD"}));
        assert_eq!(known(&tariff, &usage(0, 0)), None);
    }

    #[test]
    fn receipt_rounding_is_exact_and_bounded_before_integer_conversion() {
        let maximum = 9_007_199_254_740_991_u64;
        let limit = BigRational::from_integer(maximum.into());
        let quarter = BigRational::new(1.into(), 4.into());
        let tiny = BigRational::new(1.into(), (1_u64 << 54).into());
        let one = BigRational::from_integer(1.into());
        assert_eq!(receipt_cost(&BigRational::from_integer(0.into())), Some(0));
        assert_eq!(receipt_cost(&tiny), Some(1));
        assert_eq!(receipt_cost(&one), Some(1));
        assert_eq!(receipt_cost(&(&one - &tiny)), Some(1));
        // f64 would erase this positive fraction and understate the receipt.
        assert_eq!((&one + &tiny).to_f64(), Some(1.0));
        assert_eq!(receipt_cost(&(&one + &tiny)), Some(2));
        assert_eq!(receipt_cost(&(-tiny)), None);
        assert_eq!(receipt_cost(&(&limit - &quarter)), Some(maximum));
        assert_eq!(receipt_cost(&limit), Some(maximum));
        // f64 would hide the excess and incorrectly accept this overflow.
        assert_eq!((&limit + &quarter).to_f64(), Some(maximum as f64));
        assert_eq!(receipt_cost(&(&limit + &quarter)), None);
        assert_eq!(receipt_cost(&(&limit + &one)), None);
        assert_eq!(
            receipt_cost(&BigRational::from_integer(u64::MAX.into())),
            None
        );
        assert_eq!(
            receipt_cost(&BigRational::from_integer(u128::MAX.into())),
            None
        );
    }
}
