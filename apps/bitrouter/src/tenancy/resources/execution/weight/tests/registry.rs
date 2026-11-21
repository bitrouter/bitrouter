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
