use super::*;
use bitrouter_ai::tenancy::{
    authority::{TenantPut, TenantState},
    identity::TenantId,
};
use bitrouter_sdk::{control::ControlClient, tenancy::TenancyClient};

#[tokio::test]
async fn tenancy_owner_preserves_health_reports_and_durable_restart() -> Result<()> {
    let provider = wiremock::MockServer::start().await;
    let mut host = Host::start_profile(&provider.uri(), true)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let endpoint = format!("http://{}", host.remote);
        let client = TenancyClient::new(&endpoint, CONTROL_TOKEN)?;
        let before = client.capabilities().await?;
        ensure!(!before.dispatch_enabled && !before.strict_receipts && !before.revision_bound_dispatch);
        let tenant: TenantId = "lifecycle-tenant".try_into()?;
        let input = TenantPut {
            operation_id: "durable-host-operation".try_into()?, expected_revision: None,
            state: TenantState::Active, installation_id: before.installation_id.clone(),
        };
        let applied = client.put_tenant(&tenant, &input).await?;
        // The owner is a distinct OS process, so this also exercises the
        // advisory lock across processes, not merely between local handles.
        let file = host.home.join("host.db");
        ensure!(bitrouter::db::connect(&format!("sqlite://{}?mode=rw", file.display())).await.is_err());
        let reader = bitrouter::db::connect(&format!("sqlite://{}?mode=ro", file.display())).await?;
        reader.close().await?;
        let report = ControlClient::new(&endpoint, CONTROL_TOKEN)?.requests(Some(1)).await?;
        ensure!(report.rows.is_empty());
        let response = host.client.post(format!("http://{}/v1/chat/completions", host.inference))
            .bearer_auth(CONTROL_TOKEN)
            .json(&json!({"model": "lifecycle-model", "messages": [{"role": "user", "content": "must not execute"}]}))
            .send().await?;
        ensure!(response.status() == reqwest::StatusCode::CONFLICT);
        let error: bitrouter_ai::tenancy::operation::Error = response.json().await?;
        ensure!(error.dispatch_state == bitrouter_ai::tenancy::operation::DispatchState::NotStarted);
        ensure!(provider.received_requests().await.context("mock recording unavailable")?.is_empty());
        ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
        ensure!(host.exited().await?.success());
        host.ensure_closed().await?;
        host.spawn(&host.home.join("private.yaml"))?;
        host.ready().await?;
        let after = client.capabilities().await?;
        ensure!(after.installation_id == before.installation_id);
        ensure!(after.instance_id != before.instance_id);
        let replay = client.put_tenant(&tenant, &input).await?;
        ensure!(replay.authority_revision == applied.authority_revision);
        ensure!(replay.resource_revision == applied.resource_revision);
        let recovered = client.operation(&before.installation_id, &input.operation_id).await?;
        ensure!(recovered.resource_revision == applied.resource_revision);
        ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
        ensure!(host.exited().await?.success());
        host.ensure_closed().await?;
        Ok(())
    }).await.context("tenancy host lifecycle case timed out").and_then(|result| result);
    host.finish(result)
}
