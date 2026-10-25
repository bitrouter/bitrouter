use super::*;
use bitrouter_ai::tenancy::{
    authority::{TenantPut, TenantState},
    identity::TenantId,
};
use bitrouter_sdk::{control::ControlClient, tenancy::TenancyClient};

#[tokio::test]
async fn retained_configuration_survives_source_removal_and_rejects_wrong_key() -> Result<()> {
    use base64::Engine;
    use bitrouter_ai::tenancy::{
        configuration::{Activation, ConfigurationPut},
        identity::RevisionId,
    };
    let provider = wiremock::MockServer::start().await;
    let mut host = Host::start_profile(&provider.uri(), true)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        host.ready().await?;
        let endpoint = format!("http://{}", host.remote);
        let client = TenancyClient::new(&endpoint, CONTROL_TOKEN)?;
        let capabilities = client.capabilities().await?;
        let installation = &capabilities.installation_id;
        let tenant: TenantId = "retained-tenant".try_into()?;
        let created = client.put_tenant(&tenant, &TenantPut {
            operation_id: "create-retained-tenant".try_into()?, expected_revision: None,
            state: TenantState::Active, installation_id: installation.clone(),
        }).await?;
        ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
        ensure!(host.exited().await?.success());
        host.ensure_closed().await?;
        let reference = |kind: &str, id: &str| json!({
            "owner_kind":"tenant", "owner_id":tenant, "resource_kind":kind,
            "resource_id":id, "revision_id":"v1",
        });
        let slot = reference("credential", "mock-slot");
        let route = reference("route", "visible:opaque");
        let manifest = json!({
            "format":1, "installation_id":installation,
            "resources":[
                {"reference":slot, "definition":{"kind":"credential", "credential":{"type":"api_key", "data":{"value":PROVIDER_KEY}}}},
                {"reference":route, "definition":{
                    "kind":"route", "selector":"mock:model", "policy":null,
                    "routing":{"providers":{"mock":{"api_base":provider.uri(), "models":[{"id":"model"}]}}},
                    "credentials":[{"provider":"mock", "account":"default", "credential":slot}],
                }},
            ],
        });
        let manifest_path = host.home.join("resources.json");
        fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
        let config = host.home.join("private.yaml");
        let source = fs::read_to_string(&config)?.replace(
            "tenancy: {enabled: true}",
            "tenancy:\n  enabled: true\n  resource_import: resources.json\n  wrapping_key_env: TEST_TENANCY_WRAPPING_KEY",
        );
        fs::write(&config, &source)?;
        let key = base64::engine::general_purpose::STANDARD.encode([29; 32]);
        host.spawn_with_wrapping_key(&config, Some(&key))?;
        host.ready().await?;
        let input: ConfigurationPut = serde_json::from_value(json!({
            "operation_id":"draft-imported", "installation_id":installation,
            "expected_authority_revision":created.authority_revision,
            "document":{
                "routes":[route], "policy":null,
                "generation_defaults":{"max_output_tokens":64, "temperature":0.0},
                "learning":{"mode":"off", "max_evaluations":0},
                "limits":{"active_executions":1, "queued_work":0, "learning_jobs":0},
            },
        }))?;
        let draft = client.put_configuration(&tenant, &input).await?;
        let operation = client.operation(installation, &input.operation_id).await?;
        client.activate(&tenant, &Activation {
            operation_id:"activate-imported".try_into()?, expected_active_revision:None,
            revision_id:draft.revision_id.clone(),
            expected_authority_revision:operation.authority_revision.context("missing draft authority")?,
            installation_id:installation.clone(),
        }).await?;
        ensure!(client.configuration(installation, &tenant, &draft.revision_id).await?.document == input.document);
        ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
        ensure!(host.exited().await?.success());
        host.ensure_closed().await?;
        fs::remove_file(manifest_path)?;
        fs::write(&config, source.replace("  resource_import: resources.json\n", ""))?;
        let wrong = base64::engine::general_purpose::STANDARD.encode([30; 32]);
        for key in [None, Some(wrong.as_str())] {
            host.spawn_with_wrapping_key(&config, key)?;
            ensure!(!host.exited().await?.success());
            host.ensure_closed().await?;
        }
        host.spawn_with_wrapping_key(&config, Some(&key))?;
        host.ready().await?;
        let active: Option<RevisionId> = client.active_revision(installation, &tenant).await?.revision_id;
        ensure!(active.as_ref() == Some(&draft.revision_id));
        ensure!(client.configuration(installation, &tenant, &draft.revision_id).await?.document == input.document);
        ensure!(!client.capabilities().await?.dispatch_enabled);
        ensure!(provider.received_requests().await.context("mock recording unavailable")?.is_empty());
        ensure!(matches!(host.command(&DaemonCommand::Stop).await?, DaemonResponse::Ok));
        ensure!(host.exited().await?.success());
        host.ensure_closed().await?;
        Ok(())
    }).await.context("retained configuration host lifecycle case timed out").and_then(|result| result);
    host.finish(result)
}

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
