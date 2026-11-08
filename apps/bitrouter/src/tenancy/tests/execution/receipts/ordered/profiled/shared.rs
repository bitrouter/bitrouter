//! Group identity across real tenant, credential, HTTP and streaming boundaries.
use super::*;
use bitrouter_ai::tenancy::{
    admission_v2::AdmissionProfileV2,
    authority::{TenantPut, TenantState},
    resources_v2::{DefinitionV2, ResourcePutV2},
};

mod boundaries;

pub(super) fn grouped(rpm: u32) -> Result<AdmissionProfileV2> {
    let local = rules(rpm, true)?;
    let mut health = local.health.context("health")?;
    health.failure_threshold = 3;
    Ok(AdmissionProfileV2 {
        schema_version: 2,
        rate: local.rate,
        observe_rate_group_id: None,
        health: Some(health),
        health_group_id: Some("shared-circuit".try_into()?),
        fallback: local.fallback,
    })
}

fn move_tenant(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("owner_kind") == Some(&json!("tenant")) {
                map.insert("owner_id".into(), json!("tenant-b"));
            }
            for nested in map.values_mut() {
                move_tenant(nested);
            }
        }
        Value::Array(values) => {
            for value in values {
                move_tenant(value);
            }
        }
        _ => {}
    }
}

pub(super) async fn harness(
    profiles: &[AdmissionProfileV2],
    all_success: bool,
    two_tenants: bool,
) -> Result<OrderedHarness> {
    let old = vec![rules(1000, false)?; profiles.len()];
    OrderedHarness::with_resource_edits(profiles.len(), 8, None, Value::Null, Some(&old), |rows| {
        for (index, profile) in profiles.iter().enumerate() {
            let expected = reference(index)?;
            let row = rows
                .iter_mut()
                .find(|row| row["reference"] == json!(expected))
                .context("profile")?;
            row["definition"] = json!({"kind":"admission_profile_v2","profile":profile});
        }
        if all_success {
            let route = rows
                .iter_mut()
                .find(|row| row["reference"]["resource_kind"] == "route")
                .context("route")?;
            let routing = &mut route["definition"]["routing"];
            for provider in routing["providers"]
                .as_object_mut()
                .context("providers")?
                .values_mut()
            {
                provider["models"][0]["id"] = json!("model-0");
            }
            for endpoint in routing["models"]["ordered-root"]["endpoints"]
                .as_array_mut()
                .context("endpoints")?
            {
                endpoint["service_id"] = json!("model-0");
            }
        }
        if two_tenants {
            let mut sibling = rows
                .iter()
                .filter(|row| row["reference"]["owner_kind"] == "tenant")
                .cloned()
                .collect::<Vec<_>>();
            for row in &mut sibling {
                move_tenant(row);
            }
            rows.extend(sibling);
        }
        Ok(())
    })
    .await
}

async fn second_tenant(
    harness: &OrderedHarness,
    count: usize,
) -> Result<(
    Fixture,
    ConfigurationRevision,
    ResourceRef,
    CredentialIssued,
)> {
    let first = &harness.fixture;
    let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant-b".try_into()?;
    first
        .service
        .execute(
            first.operator.clone(),
            Command::Tenant(
                tenant.clone(),
                TenantPut {
                    operation_id: "shared-second-tenant".try_into()?,
                    installation_id: first.installation.clone(),
                    expected_revision: None,
                    state: TenantState::Active,
                },
            ),
        )
        .await
        .map_err(failure)?;
    let scope = GrantScope::Tenant {
        operator_pools: (0..count)
            .map(|n| reference(n).map(|r| r.key()))
            .collect::<Result<_>>()?,
    };
    let binding_result = match first
        .service
        .execute(
            first.operator.clone(),
            Command::Binding(
                tenant.clone(),
                first.binding.clone(),
                BindingPut {
                    operation_id: "shared-second-binding".try_into()?,
                    installation_id: first.installation.clone(),
                    expected_revision: None,
                    principal_id: first.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: scope.clone(),
                    expires_at: crate::tenancy::tests::expiry()?,
                    expected_management_owner: None,
                },
            ),
        )
        .await
        .map_err(failure)?
    {
        crate::tenancy::store::StoredResponse::Operation(result) => result,
        _ => anyhow::bail!("second binding"),
    };
    let other = Fixture {
        service: first.service.clone(),
        db: first.db.clone(),
        operator: first.operator.clone(),
        installation: first.installation.clone(),
        tenant,
        principal: first.principal.clone(),
        binding: first.binding.clone(),
        binding_result,
    };
    let mut route = harness.route.clone();
    route.owner_id = other.tenant.as_str().try_into()?;
    let revision = draft(&other, &route, "shared-second-draft", 25).await?;
    let mut issuance = other.issuance("shared-second-key").await?;
    issuance.scope = scope;
    let credential = issued(
        other
            .service
            .execute(
                other.operator.clone(),
                Command::Issue(other.tenant.clone(), issuance),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok((other, revision, route, credential))
}

async fn binding(
    harness: &OrderedHarness,
    fixture: &Fixture,
    revision: &ConfigurationRevision,
    route: &ResourceRef,
    index: u16,
    protocol: GenerationProtocol,
) -> Result<OrderedDispatchBindingV2> {
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let manifest = control
        .candidate_manifest(
            &fixture.installation,
            &fixture.tenant,
            &revision.revision_id,
            route,
            protocol,
        )
        .await?;
    ensure!(
        manifest.candidates[usize::from(index)]
            .credential
            .owner_id
            .as_str()
            == fixture.tenant.as_str()
    );
    Ok(control.ordered_capabilities().await?.bind(
        revision.revision_id.clone(),
        "shared-correlation".try_into()?,
        OrderedCandidateSelectionV1 {
            schema_version: 1,
            admission_mode: manifest.admission_mode,
            route: route.clone(),
            candidate_indices: vec![index],
            generation_defaults_digest: None,
        },
    )?)
}

async fn invoke(
    harness: &OrderedHarness,
    token: &str,
    binding: &OrderedDispatchBindingV2,
    protocol: GenerationProtocol,
    stream: bool,
    version: HttpVersion,
) -> Result<ExecutionId> {
    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: version,
            ..Default::default()
        },
    )?;
    let call = client.ordered_tenant(token, binding)?;
    macro_rules! send {
        ($method:ident,$request:ty,$body:expr,$end:expr) => {{
            let reply = call
                .$method(&serde_json::from_value::<$request>($body)?)
                .await?;
            ensure!(reply.metadata.status == 200);
            let id = reply.metadata.execution_id.context("execution")?;
            match reply.body {
                Body::Json(body) => {
                    ensure!(!stream && serde_json::to_string(&body)?.contains("receipt reply"))
                }
                Body::Stream(mut events) => {
                    ensure!(stream);
                    let mut terminal = None;
                    while let Some(event) = events.next().await {
                        if let StreamItem::End(end) = event? {
                            ensure!(terminal.is_none());
                            terminal = Some(end);
                        }
                    }
                    ensure!(terminal == Some($end));
                }
            }
            id
        }};
    }
    Ok(match protocol {
        GenerationProtocol::ChatCompletions => send!(
            chat_reply,
            ChatRequest,
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
            ProtocolEnd::ChatDone
        ),
        GenerationProtocol::Messages => send!(
            messages_reply,
            MessagesRequest,
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":stream}),
            ProtocolEnd::MessagesStop
        ),
        GenerationProtocol::Responses => send!(
            responses_reply,
            ResponsesRequest,
            json!({"model":MODEL,"input":"hello","stream":stream}),
            ProtocolEnd::ResponsesCompleted
        ),
    })
}

async fn refused(
    harness: &OrderedHarness,
    fixture: &Fixture,
    result: Result<ExecutionId>,
    status: u16,
    attempts: usize,
) -> Result<()> {
    let error = result.err().context("expected refusal")?;
    let Some(bitrouter_sdk::gateway::tenancy::Error::Response(failure)) = error.downcast_ref()
    else {
        anyhow::bail!("expected HTTP refusal: {error}");
    };
    ensure!(failure.metadata.status == status);
    let id = failure
        .metadata
        .execution_id
        .as_ref()
        .context("refusal execution")?;
    let receipt = harness
        .control(true, HttpVersion::Http1Only)?
        .execution_v2(&fixture.installation, &fixture.tenant, id)
        .await?;
    ensure!(receipt.outcome == Outcome::Failed && receipt.attempts.len() == attempts);
    Ok(())
}

#[tokio::test]
async fn shared_health_two_tenants_distinct_ceilings_actual_h1_h2_json_sse_matrix() -> Result<()> {
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        for version in [
            HttpVersion::Http1Only,
            #[cfg(feature = "http2")]
            HttpVersion::Http2PriorKnowledge,
        ] {
            for stream in [false, true] {
                let harness = harness(&[grouped(1)?, grouped(3)?], true, true).await?;
                let (other, revision, route, credential) = second_tenant(&harness, 2).await?;
                let low = binding(
                    &harness,
                    &harness.fixture,
                    &harness.revision,
                    &harness.route,
                    0,
                    protocol,
                )
                .await?;
                let high = binding(&harness, &other, &revision, &route, 1, protocol).await?;
                let token = credential.secret.as_deref().context("second token")?;
                for (fixture, token, selection) in [
                    (&harness.fixture, harness.token()?, &low),
                    (&other, token, &high),
                    (&other, token, &high),
                ] {
                    let id = invoke(&harness, token, selection, protocol, stream, version).await?;
                    let receipt = harness
                        .control(true, version)?
                        .execution_v2(&fixture.installation, &fixture.tenant, &id)
                        .await?;
                    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
                    ensure!(
                        receipt.attempts[0].candidate_index
                            == selection.selection.candidate_indices[0]
                    );
                }
                refused(
                    &harness,
                    &harness.fixture,
                    invoke(&harness, harness.token()?, &low, protocol, stream, version).await,
                    429,
                    0,
                )
                .await?;
                refused(
                    &harness,
                    &other,
                    invoke(&harness, token, &high, protocol, stream, version).await,
                    429,
                    0,
                )
                .await?;
                ensure!(harness.observed()? == ["model-0", "model-0", "model-0"]);
                for (fixture, revision, route) in [
                    (&harness.fixture, &harness.revision, &harness.route),
                    (&other, &revision, &route),
                ] {
                    let observation = harness
                        .control(true, version)?
                        .candidate_manifest(
                            &fixture.installation,
                            &fixture.tenant,
                            &revision.revision_id,
                            route,
                            protocol,
                        )
                        .await?
                        .runtime_observation
                        .context("observation")?;
                    ensure!(observation.candidates.len() == 2);
                    for (index, candidate) in observation.candidates.iter().enumerate() {
                        let rate = candidate.rate.as_ref().context("rate")?;
                        ensure!(
                            rate.shared_count == 3
                                && rate.member_rpm == if index == 0 { 1 } else { 3 }
                        );
                        ensure!(
                            candidate.health == HealthObservation::Healthy
                                && candidate.availability == AvailabilityObservation::RateLimited
                        );
                    }
                }
                harness.fixture.service.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn shared_health_distributed_failures_block_other_member_but_bypass_cannot_close_group()
-> Result<()> {
    let mut bypass = grouped(100)?;
    bypass.rate = None;
    bypass.health = None;
    bypass.health_group_id = None;
    bypass.observe_rate_group_id = Some("shared-rpm".try_into()?);
    let harness = harness(&[bypass, grouped(1)?, grouped(3)?], false, false).await?;
    let protocol = GenerationProtocol::ChatCompletions;
    let mut observed = Vec::new();
    for index in [1, 2, 2] {
        let selection = binding(
            &harness,
            &harness.fixture,
            &harness.revision,
            &harness.route,
            index,
            protocol,
        )
        .await?;
        refused(
            &harness,
            &harness.fixture,
            invoke(
                &harness,
                harness.token()?,
                &selection,
                protocol,
                false,
                HttpVersion::Http1Only,
            )
            .await,
            503,
            1,
        )
        .await?;
        observed.push(format!("model-{index}"));
    }
    for index in [1, 2] {
        let selection = binding(
            &harness,
            &harness.fixture,
            &harness.revision,
            &harness.route,
            index,
            protocol,
        )
        .await?;
        refused(
            &harness,
            &harness.fixture,
            invoke(
                &harness,
                harness.token()?,
                &selection,
                protocol,
                false,
                HttpVersion::Http1Only,
            )
            .await,
            503,
            0,
        )
        .await?;
    }
    ensure!(harness.observed()? == observed);
    let selection = binding(
        &harness,
        &harness.fixture,
        &harness.revision,
        &harness.route,
        0,
        protocol,
    )
    .await?;
    for stream in [false, true] {
        let id = invoke(
            &harness,
            harness.token()?,
            &selection,
            protocol,
            stream,
            HttpVersion::Http1Only,
        )
        .await?;
        ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
        let observation = manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?;
        ensure!(
            observation.candidates[0].health == HealthObservation::Disabled
                && observation.candidates[0].observed_shared_count == Some(3)
        );
        for member in &observation.candidates[1..] {
            ensure!(
                member.health == HealthObservation::Blocked
                    && member.availability == AvailabilityObservation::CircuitBlocked
            );
            ensure!(member.rate.as_ref().context("rate")?.shared_count == 3);
        }
    }
    // An authenticated RPM-only rotation cannot split or reset the shared circuit.
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let caps = control
        .resource_capabilities_v2(&harness.fixture.installation)
        .await?;
    ensure!(caps.provisioning_enabled && caps.admission_profile_versions == [1, 2]);
    let rotated = control
        .put_resource_v2(
            &harness.fixture.tenant,
            &ResourcePutV2 {
                format: 2,
                operation_id: "shared-rpm-rotation".try_into()?,
                installation_id: harness.fixture.installation.clone(),
                key: reference(1)?.key(),
                expected_revision: Some("r1".try_into()?),
                definition: DefinitionV2::AdmissionProfileV2 {
                    profile: grouped(100)?,
                },
            },
        )
        .await?;
    ensure!(rotated.resource_revision.is_some());
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(
        observation.candidates[1]
            .rate
            .as_ref()
            .context("rate")?
            .member_rpm
            == 100
    );
    ensure!(
        observation.candidates[1].health == HealthObservation::Blocked
            && observation.candidates[2].health == HealthObservation::Blocked
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}
