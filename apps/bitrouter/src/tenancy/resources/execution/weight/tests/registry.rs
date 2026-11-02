use super::*;
use crate::gateway::pipeline::stream::{UsagePricing, UsagePricingBracket};

fn tariff(rate: f64) -> UsagePricing {
    UsagePricing {
        registry_price: None,
        base: UsagePricingBracket {
            input_micro_usd_per_token: Some(rate),
            output_micro_usd_per_token: Some(0.0),
            ..Default::default()
        },
        context_tiers: Vec::new(),
    }
}

#[test]
fn registry_heap_cannot_bypass_owned_accounting() -> TestResult<()> {
    let mut data = route("chosen", "short")?;
    let candidate = data.candidates.first_mut().context("candidate")?;
    ensure!(Weight::new().target(&candidate.target).is_ok());
    candidate.target.registry = Some(Arc::new(
        crate::gateway::catalog::execution::SelectedInterface {
            provider_id: candidate.target.provider_name.clone(),
            provider_model_id: candidate.target.service_id.clone(),
            tariff_endpoint: Some(candidate.target.api_base.clone()),
            verified_profile: None,
            billing: None,
            interface: registry_json::json!({"private": "p".repeat(4096)}),
            auth: registry_json::json!({"kind": "none"}),
            canonical_id: None,
        },
    ));
    assert_eq!(
        Weight::new().target(&candidate.target),
        Err(ErrorCode::AuthorityUnavailable)
    );
    candidate.target.registry = None;
    data.block_pricing.push(Some(tariff(1.0)));
    ensure!(Weight::new().route(&data).is_ok());
    data.block_pricing
        .first_mut()
        .and_then(Option::as_mut)
        .context("tariff")?
        .registry_price = Some(registry_json::json!({"private": "p".repeat(4096)}));
    assert_eq!(
        Weight::new().route(&data),
        Err(ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
fn cost_for_receipt(rates: [f64; 2]) -> TestResult<Option<u64>> {
    use bitrouter_ai::tenancy::execution::{Attempt, Outcome, Provenance, Usage};

    let mut data = route("chosen", "short")?;
    let candidate = data.candidates.first().context("candidate")?;
    let mut second = Candidate {
        routing: Default::default(),
        target: candidate.target.clone(),
        projection: candidate.projection.clone(),
        credential: candidate.credential.clone(),
    };
    // Public receipts omit protocol: both candidates have the same identity.
    second.target.api_protocol = ApiProtocol::Responses;
    second.projection = super::super::super::target::project(&second.target)?;
    let mut value = revision()?;
    let target = crate::tenancy::receipts::journal::project_target(
        &value.installation,
        &data.reference,
        &candidate.credential,
        &candidate.target.provider_name,
        &candidate.target.service_id,
        candidate.target.account_label.as_deref(),
    )?;
    data.candidates.push(second);
    data.chains.first_mut().context("chain")?.candidates.push(1);
    data.block_pricing = rates.into_iter().map(|rate| Some(tariff(rate))).collect();
    insert(&mut value, "chosen", data);
    let bytes = weight(&value)?;
    let cache = Cache::<u8, RetainedExecutionRevision>::new(1, 1024 * 1024)?;
    let root = cache.insert(1, value, bytes, 0)?;
    let route = root
        .select("chosen", &ApiProtocol::ChatCompletions)
        .map_err(|code| anyhow::anyhow!("selection failed: {code:?}"))?;
    let attempt = Attempt {
        attempt_id: "attempt".try_into()?,
        target: Some(target),
        outcome: Outcome::Completed,
        usage: Usage {
            input_tokens: Some(1),
            output_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            reasoning_tokens: Some(0),
            provenance: Provenance::Provider,
            complete: true,
        },
        native_usage: Vec::new(),
    };
    Ok(route.block_token_cost("fixture:model", &attempt))
}

#[cfg(all(feature = "core", feature = "harness"))]
#[test]
fn receipt_candidates_agree_before_rounding_and_respect_integer_limit() -> TestResult<()> {
    // Equal ceilings do not prove equal exact tariff results.
    assert_eq!(cost_for_receipt([1.25, 1.5])?, None);
    assert_eq!(cost_for_receipt([1.25, 1.25])?, Some(2));
    assert_eq!(cost_for_receipt([0.0, 0.0])?, Some(0));
    let maximum = 9_007_199_254_740_991_u64;
    assert_eq!(cost_for_receipt([maximum as f64; 2])?, Some(maximum));
    assert_eq!(cost_for_receipt([(maximum + 1) as f64; 2])?, None);
    assert_eq!(cost_for_receipt([f64::MAX; 2])?, None);
    Ok(())
}
