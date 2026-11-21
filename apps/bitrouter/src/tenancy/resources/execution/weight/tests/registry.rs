use super::*;
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
    Ok(())
}
