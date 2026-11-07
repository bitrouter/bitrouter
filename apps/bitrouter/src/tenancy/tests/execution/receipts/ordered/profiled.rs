//! Profiled admission uses actual SDK sockets and durable original receipts.
use super::*;
use crate::tenancy::{command::Command, model::Caller, tests::issued};
use bitrouter_ai::tenancy::{
    admission::{AdmissionProfileV1, AvailabilityObservation, HealthObservation},
    authority::{BindingPut, GrantScope, TenantRole},
    operation::{DispatchState, ErrorCode},
};

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
mod telemetry;

mod boundaries;
mod observation;

pub(super) fn reference(index: usize) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "operational-pool".try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: format!("profile-{index}").try_into()?,
        revision_id: "r1".try_into()?,
    })
}

pub(super) async fn grant_pool(
    fixture: &Fixture,
    references: &[ResourceRef],
) -> Result<CredentialIssued> {
    let binding = crate::tenancy::store::directory::binding_in(
        &fixture.db,
        &fixture.tenant,
        &fixture.binding,
        64 * 1024,
    )
    .await?
    .context("binding")?;
    // Resource edits may share profiles across many candidate ordinals. Grant
    // every retained pool key, including unselected profiles, without inventing
    // keys for profiles the fixture no longer imports.
    let mut operator_pools = Vec::new();
    for reference in references.iter().filter(|reference| {
        reference.owner_kind == OwnerKind::OperatorPool
            && reference.resource_kind == ResourceKind::Policy
    }) {
        let key = reference.key();
        if !operator_pools.contains(&key) {
            operator_pools.push(key);
        }
    }
    let scope = GrantScope::Tenant { operator_pools };
    let Caller::Installation(owner) = &fixture.operator else {
        anyhow::bail!("operator");
    };
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Binding(
                fixture.tenant.clone(),
                fixture.binding.clone(),
                BindingPut {
                    operation_id: "operational-pool-grant".try_into()?,
                    expected_revision: Some(binding.revision.clone()),
                    principal_id: fixture.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: scope.clone(),
                    expires_at: binding.expires.wire.clone(),
                    expected_management_owner: Some(owner.clone()),
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let mut input = fixture.issuance("ordered-client").await?;
    input.scope = scope;
    issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )
}

pub(super) async fn draft_profiles(
    fixture: &Fixture,
    route: &ResourceRef,
) -> Result<ConfigurationRevision> {
    let mut input = fixture.configuration_input(&format!("draft-{}", route.resource_id))?;
    input.document.routes.push(route.clone());
    input.document.generation_defaults.max_output_tokens = 25;
    input.document.limits.active_executions = 32;
    input.document.limits.queued_work = 32;
    match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        crate::tenancy::store::StoredResponse::Configuration(revision) => Ok(revision),
        _ => anyhow::bail!("expected profiled configuration"),
    }
}

fn rules(rpm: u32, health: bool) -> Result<AdmissionProfileV1> {
    Ok(serde_json::from_value(json!({"schema_version":1,
        "rate":{"rate_group_id":"shared-rpm","member_rpm":rpm},
        "health": if health { json!({"rules":"availability_v1","failure_threshold":1,"transient_cooldown_seconds":60,
            "persistent_cooldown_seconds":120,"probe_lease_seconds":5}) } else { Value::Null },
        "fallback":"ordered_terminal_rejections_v1"}))?)
}

async fn manifest(
    harness: &OrderedHarness,
) -> Result<bitrouter_ai::tenancy::ordered::CandidateManifestV1> {
    Ok(harness
        .control(true, HttpVersion::Http1Only)?
        .candidate_manifest(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &harness.revision.revision_id,
            &harness.route,
            GenerationProtocol::ChatCompletions,
        )
        .await?)
}

#[tokio::test]
async fn profiled_exact_orders_charge_only_actual_reached_hops_and_retain_ordinals() -> Result<()> {
    for count in [1_u16, 8, 9, 64] {
        let profiles = vec![rules(1000, false)?; usize::from(count)];
        let harness = OrderedHarness::with_profiles(
            usize::from(count),
            64,
            None,
            Value::Null,
            Some(&profiles),
        )
        .await?;
        let order: Vec<_> = (0..count).rev().collect();
        let binding = harness
            .binding(order.clone(), GenerationProtocol::ChatCompletions)
            .await?;
        let id = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.outcome == Outcome::Completed);
        ensure!(
            receipt
                .attempts
                .iter()
                .map(|attempt| attempt.candidate_index)
                .collect::<Vec<_>>()
                == order
        );
        ensure!(
            harness.observed()?
                == order
                    .iter()
                    .map(|index| format!("model-{index}"))
                    .collect::<Vec<_>>()
        );
        for observed in manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates
        {
            ensure!(observed.rate.context("rate")?.shared_count == u64::from(count));
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn profiled_json_sse_three_protocols_h1_h2_share_one_process_window() -> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(1000, true)?])).await?;
    let mut calls = 0;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        let binding = harness.binding(vec![0], protocol).await?;
        for version in [
            HttpVersion::Http1Only,
            #[cfg(feature = "http2")]
            HttpVersion::Http2PriorKnowledge,
        ] {
            for stream in [false, true] {
                let id = harness.invoke(&binding, protocol, stream, version).await?;
                ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
                calls += 1;
            }
        }
    }
    let observed = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?
        .candidates
        .remove(0);
    ensure!(observed.health == HealthObservation::Healthy);
    ensure!(observed.rate.context("rate")?.shared_count == calls);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_local_zero_then_terminal_rejection_falls_back_without_inventing_attempts()
-> Result<()> {
    let profiles = [rules(1000, false)?, rules(1000, false)?, rules(0, false)?];
    let harness = OrderedHarness::with_profiles(3, 8, None, Value::Null, Some(&profiles)).await?;
    let binding = harness
        .binding(vec![2, 1, 0], GenerationProtocol::ChatCompletions)
        .await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == [1, 0]
    );
    ensure!(harness.observed()? == ["model-1", "model-0"]);
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(observation.candidates[2].availability == AvailabilityObservation::RateLimited);
    ensure!(
        observation.candidates[0]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 2
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_sixteen_actual_requests_race_for_one_shared_entry() -> Result<()> {
    const REQUESTS: usize = 16;
    let mut capacity = CapacityConfig::default();
    let authority_bytes_per_read = capacity.authority_views.bytes / capacity.management.reads;
    // This race targets shared RPM admission, not the default eight-read
    // overload boundary. Size every preceding runtime owner for all sixteen
    // requests and preserve the default authority reservation per read.
    capacity.ingress.requests = REQUESTS;
    capacity.execution.instance_active = REQUESTS;
    capacity.execution.tenant_active = REQUESTS;
    capacity.management.queue = REQUESTS * 2;
    capacity.management.reads = REQUESTS * 2;
    capacity.authority_views.bytes = authority_bytes_per_read * capacity.management.reads;
    let read_slots = capacity.management.reads;
    let harness = OrderedHarness::with_runtime_capacity(
        1,
        8,
        None,
        Value::Null,
        Some(&[rules(1, false)?]),
        capacity,
        |_| Ok(()),
    )
    .await?;
    ensure!(harness.fixture.service.reads.available_permits() == read_slots);
    ensure!(
        harness
            .fixture
            .service
            .execution_settings
            .capacity
            .authority_views
            .bytes
            / read_slots
            == authority_bytes_per_read
    );
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    // Exercise the shared RPM race after a normal read-only configuration
    // cache warmup. The overload test below separately checks refusals before
    // an execution or rate admission exists.
    let submission = harness
        .fixture
        .service
        .submission(&caller_timeout::ordered_headers(&harness, &binding)?)
        .await
        .map_err(failure)?;
    harness
        .fixture
        .service
        .warm_retained_execution_for_test(&submission)
        .await
        .map_err(failure)?;
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates[0]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 0
    );
    let results = futures::future::join_all((0..REQUESTS).map(|_| {
        harness.invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
    }))
    .await;
    ensure!(results.iter().filter(|result| result.is_ok()).count() == 1);
    let completed_id = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .context("completed response identity")?;
    let mut refused_ids = std::collections::BTreeSet::new();
    for result in &results {
        let Err(error) = result else {
            continue;
        };
        let dispatch = error
            .downcast_ref::<bitrouter_sdk::gateway::tenancy::Error>()
            .context("expected SDK rate refusal")?;
        let bitrouter_sdk::gateway::tenancy::Error::Response(failure) = dispatch else {
            anyhow::bail!("expected actual HTTP rate refusal: {error}");
        };
        ensure!(failure.metadata.status == 429 && failure.problem.is_none());
        ensure!(
            failure.tenancy_error.is_none(),
            "expected admitted RPM refusal, received {}",
            serde_json::to_string(&failure.tenancy_error)?
        );
        // An ordinary gateway body is not positive dispatch evidence. The
        // retained receipt below must establish every admitted outcome.
        ensure!(dispatch.dispatch_state() == DispatchState::Unknown);
        let id = failure
            .metadata
            .execution_id
            .as_ref()
            .context("admitted refusal identity")?;
        ensure!(refused_ids.insert(id.clone()));
        let Some(bitrouter_ai::gateway::GatewayErrorEnvelope {
            error: bitrouter_ai::gateway::GatewayErrorValue::Detail(detail),
            ..
        }) = &failure.gateway_error
        else {
            anyhow::bail!("expected gateway rate refusal");
        };
        ensure!(
            detail.code.as_ref().and_then(Option::as_ref) == Some(&json!("rate_limit_exceeded"))
        );
    }
    ensure!(refused_ids.len() == 15);
    ensure!(harness.observed()? == ["model-0"]);
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(
        observation.candidates[0]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 1
    );
    let page = harness
        .control(false, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.len() == 16 && page.next_after.is_none());
    let mut response_ids = refused_ids.clone();
    ensure!(response_ids.insert(completed_id.clone()));
    ensure!(
        page.execution_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            == response_ids
    );
    let mut completed = 0;
    for id in page.execution_ids {
        let receipt = harness.receipt(&id).await?;
        if receipt.outcome == Outcome::Completed {
            completed += 1;
            ensure!(&id == completed_id);
            ensure!(receipt.attempts.len() == 1);
        } else {
            ensure!(refused_ids.contains(&id));
            ensure!(receipt.outcome == Outcome::Failed && receipt.state != ReceiptState::Unknown);
            ensure!(receipt.attempts.is_empty());
        }
    }
    ensure!(completed == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_runtime_read_overload_has_no_execution_or_rate_charge() -> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(1, false)?])).await?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let reads = u32::try_from(
        harness
            .fixture
            .service
            .execution_settings
            .capacity
            .management
            .reads,
    )?;
    let held = tokio::time::timeout(TIMEOUT, harness.fixture.service.reads.acquire_many(reads))
        .await
        .context("fixture read slots did not drain")??;
    let result = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await;
    drop(held);
    let error = result.err().context("expected preadmission read refusal")?;
    let dispatch = error
        .downcast_ref::<bitrouter_sdk::gateway::tenancy::Error>()
        .context("expected SDK dispatch error")?;
    ensure!(dispatch.dispatch_state() == DispatchState::NotStarted);
    let bitrouter_sdk::gateway::tenancy::Error::Response(failure) = dispatch else {
        anyhow::bail!("expected HTTP capacity response");
    };
    ensure!(
        failure.metadata.status == 429
            && failure.metadata.execution_id.is_none()
            && failure.problem.is_none()
            && failure.gateway_error.is_none()
    );
    let body = failure.tenancy_error.as_ref().context("capacity body")?;
    ensure!(
        body.code == ErrorCode::CapacityExceeded
            && body.dispatch_state == DispatchState::NotStarted
            && body.execution_id.is_none()
            && body.operation_id.is_none()
    );
    let page = harness
        .control(false, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.is_empty() && page.next_after.is_none());
    ensure!(harness.observed()?.is_empty());
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates[0]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 0
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_invalid_success_opens_health_but_never_authorizes_fallback() -> Result<()> {
    let profiles = vec![rules(1000, true)?; 3];
    let harness =
        OrderedHarness::with_profiles(3, 8, Some(2), Value::Null, Some(&profiles)).await?;
    let binding = harness
        .binding(vec![2, 1, 0], GenerationProtocol::ChatCompletions)
        .await?;
    ensure!(
        harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only
            )
            .await
            .is_err()
    );
    ensure!(harness.observed()? == ["model-2"]);
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(observation.candidates[2].health == HealthObservation::Blocked);
    ensure!(observation.candidates[0].health == HealthObservation::Healthy);
    let page = harness
        .control(false, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness.receipt(&page.execution_ids[0]).await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.attempts.len() == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_terminal_missing_usage_is_healthy_but_keeps_incomplete_financial_evidence()
-> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(1000, true)?])).await?;
    harness.controls.missing_usage.store(true, Ordering::SeqCst);
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    for stream in [false, true] {
        let id = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                stream,
                HttpVersion::Http1Only,
            )
            .await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.outcome == Outcome::Completed && receipt.state == ReceiptState::Incomplete);
        ensure!(
            receipt.usage.cache_read_tokens.is_none()
                && receipt.usage.cache_write_tokens.is_none()
                && receipt.usage.reasoning_tokens.is_none()
        );
        let observed = manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?;
        ensure!(observed.candidates[0].health == HealthObservation::Healthy);
    }
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_persistent_status_respects_exact_fallback_rule_and_later_circuit_skip()
-> Result<()> {
    use bitrouter_ai::tenancy::admission::FallbackRule;
    for fallback in [
        FallbackRule::Stop,
        FallbackRule::OrderedTerminalRejectionsV1,
    ] {
        let mut profiles = vec![rules(1000, true)?; 2];
        profiles[1].fallback = fallback;
        let harness =
            OrderedHarness::with_profiles(2, 8, None, Value::Null, Some(&profiles)).await?;
        harness
            .controls
            .rejection_status
            .store(401, Ordering::SeqCst);
        let binding = harness
            .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
            .await?;
        let result = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await;
        let enabled = fallback == FallbackRule::OrderedTerminalRejectionsV1;
        ensure!(result.is_ok() == enabled);
        let observed = manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?;
        ensure!(observed.candidates[1].health == HealthObservation::Blocked);
        ensure!(
            harness.observed()?
                == if enabled {
                    vec!["model-1", "model-0"]
                } else {
                    vec!["model-1"]
                }
        );
        // A later local circuit refusal skips without an AttemptId, including
        // when that profile does not permit fallback after a provider rejection.
        let id = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 0);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
