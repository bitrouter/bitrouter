use super::*;
use bitrouter_ai::tenancy::operation::{DispatchState, ErrorCode};

#[tokio::test]
async fn ordered_retained_continuation_defaults_reject_before_any_provider_send() -> Result<()> {
    for field in ["context_management", "cachedContent"] {
        let defaults = serde_json::Map::from_iter([(field.into(), json!({"context":"private"}))]);
        let harness = OrderedHarness::with_defaults(1, 8, None, defaults.into()).await?;
        let control = harness.control(true, HttpVersion::Http1Only)?;
        ensure!(
            control
                .candidate_manifest(
                    &harness.fixture.installation,
                    &harness.fixture.tenant,
                    &harness.revision.revision_id,
                    &harness.route,
                    GenerationProtocol::Messages,
                )
                .await
                .is_err()
        );
        let binding = control.ordered_capabilities().await?.bind(
            harness.revision.revision_id.clone(),
            "no-continuation-defaults".try_into()?,
            OrderedCandidateSelectionV1 {
                schema_version: 1,
                route: harness.route.clone(),
                candidate_indices: vec![0],
            },
        )?;
        let client = GatewayClient::new(&harness.endpoint, None)?;
        let request: MessagesRequest = serde_json::from_value(json!({
            "model":MODEL, "messages":[{"role":"user","content":"hello"}], "max_tokens":25
        }))?;
        let error = client
            .ordered_tenant(harness.token()?, &binding)?
            .messages_reply(&request)
            .await
            .err()
            .context("continuation default admitted")?;
        let bitrouter_sdk::gateway::tenancy::Error::Response(error) = error else {
            anyhow::bail!("expected protected HTTP rejection");
        };
        ensure!(error.metadata.status == 409 && error.metadata.execution_id.is_none());
        let rejected = error.tenancy_error.context("typed tenancy rejection")?;
        ensure!(
            rejected.code == ErrorCode::CapabilityMismatch
                && rejected.dispatch_state == DispatchState::NotStarted
        );
        ensure!(harness.observed()?.is_empty());
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
