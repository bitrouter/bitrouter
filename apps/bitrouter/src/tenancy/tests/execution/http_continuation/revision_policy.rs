//! A continuation pins its backend/route, while each new execution pins its own
//! configuration and intersects the current policy-selected candidate set.
use super::*;
use crate::{
    policy_lock::{PolicyDefinition, PolicyLock},
    tenancy::{Service, command::Command, resources::Resources, store::StoredResponse},
};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{Activation, ConfigurationRevision, ResourceRef},
    operation::ErrorCode,
};

async fn configured(provider: &Server) -> Result<(Ready, ResourceRef, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let reference = |kind, name: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: name.try_into()?,
            revision_id: "r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let policy = reference(ResourceKind::Policy, "selected-policy")?;
    let first = reference(ResourceKind::Credential, "first-slot")?;
    let second = reference(ResourceKind::Credential, "second-slot")?;
    let mut document = PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        PolicyDefinition {
            tiers: [
                (
                    "economy".into(),
                    bitrouter_ai::routing::PolicyModelTarget::model_only("first:model-a".into()),
                ),
                (
                    "strong".into(),
                    bitrouter_ai::routing::PolicyModelTarget::model_only("second:model-a".into()),
                ),
            ]
            .into(),
            default_tier: Some("economy".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            ..Default::default()
        },
    );
    let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let credential = |reference: &ResourceRef| json!({"reference":reference,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}});
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            credential(&first), credential(&second),
            {"reference":policy,"definition":{"kind":"policy","name":"selected","document":document}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/choice","policy":policy,
                "routing":{"providers":{
                    "first":{"api_base":provider.url,"api_protocol":[{"*":"responses"}],"headers":{"x-candidate":"first"},"models":[{"id":"model-a"}]},
                    "second":{"api_base":provider.url,"api_protocol":[{"*":"responses"}],"headers":{"x-candidate":"second"},"models":[{"id":"model-a"}]}},
                    "routers":{"choice":{"selection":{"kind":"policy","policy":"selected","base_model":"first:model-a"}}}},
                "credentials":[{"provider":"first","account":"default","credential":first},{"provider":"second","account":"default","credential":second}]}}
        ]
    }))?).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        Default::default(),
        crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?,
        Arc::new(resources),
    )
    .await?;
    let revision = revision(&fixture, &route, &policy, "policy-config-r1", 32).await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let issued = fixture.issue("policy-caller").await?;
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("caller")?,
        &revision.revision_id,
    )?;
    Ok((
        Ready {
            fixture,
            headers,
            route,
            revision,
        },
        policy,
        second,
    ))
}
async fn revision(
    fixture: &Fixture,
    route: &ResourceRef,
    policy: &ResourceRef,
    operation: &str,
    tokens: u32,
) -> Result<ConfigurationRevision> {
    let mut input = fixture.configuration_input(operation)?;
    input.document.routes = vec![route.clone()];
    input.document.policy = Some(policy.clone());
    input.document.generation_defaults.max_output_tokens = tokens;
    input.document.limits.active_executions = 4;
    input.document.limits.queued_work = 4;
    match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(row) => Ok(row),
        _ => anyhow::bail!("expected policy configuration"),
    }
}
async fn wire(provider: &mut Server, candidate: &str) -> Result<Value> {
    let (headers, body) = tokio::time::timeout(WAIT, provider.received.recv())
        .await?
        .context("provider request")?;
    ensure!(
        headers
            .get("authorization")
            .is_some_and(|h| h == "Bearer owned-upstream-secret")
    );
    ensure!(headers.get("x-candidate").is_some_and(|h| h == candidate));
    Ok(body)
}
fn with_tools(mut input: Value) -> Value {
    input["tools"] = json!([{"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}]);
    input
}

#[tokio::test]
async fn same_route_new_configuration_resumes_with_new_defaults_but_current_policy_can_deny_old_target()
-> Result<()> {
    for stream in [false, true] {
        let mut provider = Server::start().await?;
        let (ready, policy, second_slot) = configured(&provider).await?;
        let outcome: Result<()> = async {
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(
                status == StatusCode::OK,
                "R1 failed: {}",
                String::from_utf8_lossy(&bytes)
            );
            let first_public = public_id(&bytes, stream)?;
            ensure!(wire(&mut provider, "first").await?["max_output_tokens"] == 32);
            let (crypto, row) = store::retained(&ready, &first_public).await?;
            let first_live = row.open(&crypto)?.live.context("R1 live")?;
            ensure!(first_live.revision == ready.revision.revision_id);
            ensure!(first_live.binding.credential != second_slot);
            let r2 = revision(
                &ready.fixture,
                &ready.route,
                &policy,
                "policy-config-r2",
                17,
            )
            .await?;
            ensure!(r2.revision_id != ready.revision.revision_id);
            ensure!(r2.document.routes == ready.revision.document.routes);
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Activate(
                        ready.fixture.tenant.clone(),
                        Activation {
                            operation_id: "activate-policy-r2".try_into()?,
                            expected_active_revision: None,
                            revision_id: r2.revision_id.clone(),
                            expected_authority_revision: ready
                                .fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                            installation_id: ready.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?;
            let issued = ready.fixture.issue("same-principal-r2-caller").await?;
            let r2_headers = headers(
                &ready.fixture,
                issued.secret.as_deref().context("R2 caller")?,
                &r2.revision_id,
            )?;
            let (status, bytes) = send(
                &ready,
                r2_headers.clone(),
                body(!stream, Some(&first_public)),
            )
            .await?;
            ensure!(
                status == StatusCode::OK,
                "same-route R2 resume rejected: {}",
                String::from_utf8_lossy(&bytes)
            );
            let r2_public = public_id(&bytes, !stream)?;
            let sent = wire(&mut provider, "first").await?;
            ensure!(sent["previous_response_id"] == "resp-http-private-1");
            ensure!(
                sent["max_output_tokens"] == 17,
                "R2 execution inherited R1 defaults"
            );
            let (_, row) = store::retained(&ready, &r2_public).await?;
            let r2_live = row.open(&crypto)?.live.context("R2 live")?;
            ensure!(
                r2_live.revision == r2.revision_id && r2_live.execution != first_live.execution
            );
            ensure!(r2_live.binding == first_live.binding);
            // Same immutable route/policy, but the current request's tool floor
            // selects the other approved candidate. Equal API keys do not allow
            // the old handle to escape policy's narrowed selected chain.
            let before = provider.state.calls.load(Ordering::SeqCst);
            let (status, bytes) = send(
                &ready,
                r2_headers.clone(),
                with_tools(body(false, Some(&first_public))),
            )
            .await?;
            ensure!(!status.is_success());
            let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
            ensure!(error.code == ErrorCode::InvalidRequest);
            ensure!(provider.state.calls.load(Ordering::SeqCst) == before);
            ensure!(row_count(&ready).await? == 2);
            // Positive control proves the policy branch is reachable, not a
            // malformed tool request or an unsupported provider declaration.
            let (status, bytes) = send(&ready, r2_headers, with_tools(body(false, None))).await?;
            ensure!(
                status == StatusCode::OK,
                "tool-policy control failed: {}",
                String::from_utf8_lossy(&bytes)
            );
            let second_public = public_id(&bytes, false)?;
            let fresh = wire(&mut provider, "second").await?;
            ensure!(fresh.get("previous_response_id").is_none());
            let (_, row) = store::retained(&ready, &second_public).await?;
            ensure!(
                row.open(&crypto)?
                    .live
                    .context("second live")?
                    .binding
                    .credential
                    == second_slot
            );
            ensure!(ready.fixture.service.current().is_ok());
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}
