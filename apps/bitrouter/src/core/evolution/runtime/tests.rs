use super::*;
use crate::auth::events::ApiPrincipalEstablished;
use crate::gateway::observability::binding::CanonicalSelection;
use crate::gateway::observability::inventory::{
    BeginResult, GatewayInventory, GatewayRequest, InventoryBinding,
};
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::{PreRequestHook, RouteHook};
use crate::storage::execution::{ExecutionOutcome, ExecutionSettlement};
mod execution_guards;
mod judge_costs;
mod resource_coverage;
mod revisions;
use crate::gateway::caller::CallerContext;
use crate::harness::acp::capture::CapturePort;
use crate::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

use crate::acp_trajectory::{CanonicalStore, RecordingScope};
use crate::gateway::routing::blocks::allocation::Arm;

use crate::gateway::routing::blocks::control::{BlockStatus, EvolutionMode};
use crate::gateway::routing::blocks::store::{CONTROL_KEY, CONTROL_KIND};
use crate::session_identity::{RequestSessionContext, SessionContextHook};

use crate::gateway::routing::blocks::fixtures::{
    Fixture, definition, fixture, recording, request, session,
};

async fn adopt_fixture(runtime: &EvolutionRuntime) -> Result<()> {
    let store = runtime.service("local")?.store;
    let (revision, _): (_, ControlState) = store
        .get(CONTROL_KIND, CONTROL_KEY)
        .await?
        .context("control missing")?;
    store
        .update::<ControlState, _>(CONTROL_KIND, CONTROL_KEY, revision, |state| {
            state
                .blocks
                .get_mut("coding-block")
                .context("block missing")?
                .status = BlockStatus::Adopted;
            Ok(())
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn operator_restore_changes_actual_dispatch_for_existing_and_new_sessions_while_off()
-> Result<()> {
    // The adopted setup is a controlled state fixture; this test establishes
    // withdrawal and actual dispatch, not promotion from empirical feedback.
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    adopt_fixture(runtime).await?;
    let service = runtime.service("local")?;
    let before = service.state().await?;
    assert_eq!(before.mode, EvolutionMode::Off);
    let identity = session(&fixture, "operator-restoration", "fixture").await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&identity, "adopted-request", "coding")?)
        .await?;
    let enrollment = serde_json::to_value(service.enrollment(&identity).await?)?;
    let block = before.blocks.get("coding-block").context("block missing")?;
    runtime
        .operate(
            "local",
            crate::core::evolution::operator::EvolutionOperation::Restore {
                request: crate::core::evolution::control::restoration::RestoreRequest {
                    block: "coding-block".into(),
                    expected_experiment: block.experiment_id.clone(),
                    expected_revision: block.revision.clone(),
                    reason: "Restore through the shared operator API".into(),
                },
            },
        )
        .await?;
    pipeline
        .execute(request(&identity, "restored-request", "coding")?)
        .await?;
    let fresh = session(&fixture, "after-operator-restoration", "fixture").await?;
    pipeline
        .execute(request(&fresh, "fresh-restored-request", "coding")?)
        .await?;
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests unavailable")?;
    let models = requests
        .iter()
        .map(|r| Ok(serde_json::from_slice::<Value>(&r.body)?["model"].clone()))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        models,
        vec![json!("cheap"), json!("strong"), json!("strong")]
    );
    assert_eq!(
        serde_json::to_value(service.enrollment(&identity).await?)?,
        enrollment
    );
    assert_eq!(service.state().await?.mode, EvolutionMode::Off);
    let executions = runtime.executions(&identity).await?;
    assert_eq!(executions.len(), 2);
    assert_eq!(executions[0].dispatch_route, "candidate");
    assert_eq!(executions[1].dispatch_route, "coding");
    Ok(())
}

#[tokio::test]
async fn assembled_requests_apply_sticky_arms_and_record_actual_execution_once() -> Result<()> {
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    let service = runtime.service("local")?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = session(&fixture, "sticky", "fixture").await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&identity, "request-1", "coding")?)
        .await?;
    let enrollment = service
        .enrollment(&identity)
        .await?
        .context("enrollment missing")?;
    let assignment = enrollment
        .assignments
        .get("coding-block")
        .context("assignment missing")?;
    let expected_model = if assignment.arm == Arm::Challenger {
        "cheap"
    } else {
        "strong"
    };
    pipeline
        .execute(request(&identity, "request-2", "coding")?)
        .await?;
    let executions = runtime.executions(&identity).await?;
    assert_eq!(executions.len(), 2);
    for execution in &executions {
        let settlement = execution
            .settlement
            .as_ref()
            .context("settlement missing")?;
        assert_eq!(settlement.hops.len(), 1);
        assert_eq!(settlement.hops[0].model, expected_model);
        assert!(settlement.total_cost_micro_usd.is_some());
        assert!(settlement.error_code.is_none());
    }
    let metered_before = crate::metering::entities::requests::Entity::find()
        .filter(crate::metering::entities::requests::Column::RequestId.eq("request-1"))
        .one(&fixture.assembled.db)
        .await?
        .context("metering row missing")?;
    assert!(
        pipeline
            .execute(request(&identity, "request-1", "coding")?)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .len(),
        2
    );
    let metered_after = crate::metering::entities::requests::Entity::find()
        .filter(crate::metering::entities::requests::Column::RequestId.eq("request-1"))
        .one(&fixture.assembled.db)
        .await?
        .context("metering row missing")?;
    assert_eq!(
        metered_before, metered_after,
        "a rejected replay must not replace successful cost evidence"
    );
    assert!(!serde_json::to_string(&executions)?.contains("secret-not-for-evidence"));
    assert_eq!(runtime.executions(&identity).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn named_candidate_keeps_preset_defaults_and_tool_safety_selection() -> Result<()> {
    let fixture = fixture(true).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("@coding", "@candidate"))
        .await?;
    adopt_fixture(runtime).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    let plain = session(&fixture, "plain", "fixture").await?;
    pipeline
        .execute(request(&plain, "plain-request", "@coding")?)
        .await?;
    let tools = session(&fixture, "tools", "fixture").await?;
    let mut with_tools = request(&tools, "tool-request", "@coding")?;
    with_tools
        .input
        .generation_prompt_mut()
        .ok_or_else(|| {
            crate::gateway::error::BitrouterError::internal("generation fixture missing")
        })?
        .tools = serde_json::from_value(json!([{
        "type":"function", "name":"read_file", "description":"Read a file", "parameters":{"type":"object"}
    }]))?;
    pipeline.execute(with_tools).await?;
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests unavailable")?;
    let plain_body: Value = serde_json::from_slice(&requests[0].body)?;
    let tool_body: Value = serde_json::from_slice(&requests[1].body)?;
    assert_eq!(plain_body["model"], "cheap");
    assert_eq!(tool_body["model"], "strong");
    assert_eq!(plain_body["temperature"], 0.2);
    assert_eq!(
        plain_body["messages"][0]["content"],
        "Use recorded requirements."
    );
    let execution = runtime
        .executions(&tools)
        .await?
        .pop()
        .context("execution missing")?;
    assert_eq!(
        execution
            .intent
            .as_ref()
            .context("intent missing")?
            .selected_route,
        "@candidate"
    );
    assert_eq!(
        execution
            .settlement
            .as_ref()
            .context("settlement missing")?
            .hops[0]
            .model,
        "strong"
    );
    Ok(())
}

#[tokio::test]
async fn fallback_cost_is_unknown_even_when_the_final_attempt_was_priced() -> Result<()> {
    let fixture = fixture(false).await?;
    let identity = session(&fixture, "fallback", "fixture").await?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request(&identity, "fallback-request", "fallback")?)
        .await?;
    let execution = fixture
        .assembled
        .evolution
        .executions(&identity)
        .await?
        .pop()
        .context("execution missing")?;
    assert!(
        execution.intent.is_none(),
        "off does not create an experiment"
    );
    let settlement = execution.settlement.context("settlement missing")?;
    assert_eq!(settlement.hops.len(), 2);
    assert_eq!(settlement.hops[0].status, "failed");
    assert_eq!(settlement.hops[1].status, "completed");
    assert!(settlement.final_metered_cost_micro_usd.is_some());
    assert!(settlement.total_cost_micro_usd.is_none());
    Ok(())
}

#[tokio::test]
async fn dependency_scope_ignores_unrelated_routes_but_dispatch_fences_aba_reload() -> Result<()> {
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    let definition = definition("coding", "candidate");
    runtime.register("local", definition.clone()).await?;
    runtime
        .service("local")?
        .set_mode(EvolutionMode::Manual, None)
        .await?;
    let identity = session(&fixture, "reload", "fixture").await?;
    let mut ctx = PipelineContext::new(request(&identity, "reload-request", "coding")?);
    SessionContextHook::default().check(&mut ctx).await?;
    fixture.assembled.gateway_requests.check(&mut ctx).await?;
    block_hook(&fixture).check(&mut ctx).await?;
    binding_hook(&fixture).check(&mut ctx).await?;
    let original = fixture.assembled.routing_table.snapshot_config();
    let policies = fixture.assembled.policy_runtime.routing_snapshot();
    let before = block_digest(&original, &policies, &definition).await?;
    let mut unrelated = original.clone();
    unrelated.models.insert(
        "unrelated".into(),
        original
            .models
            .get("fallback")
            .context("route missing")?
            .clone(),
    );
    assert_eq!(
        before,
        block_digest(&unrelated, &policies, &definition).await?
    );
    let mut relevant = original.clone();
    relevant
        .models
        .get_mut("candidate")
        .context("candidate missing")?
        .endpoints[0]
        .service_id = "strong".into();
    assert_ne!(
        before,
        block_digest(&relevant, &policies, &definition).await?
    );
    let generation = fixture.assembled.routing_table.generation();
    fixture
        .assembled
        .routing_table
        .replace_prepared_config(relevant)
        .await?;
    fixture
        .assembled
        .routing_table
        .replace_prepared_config(original)
        .await?;
    assert_eq!(fixture.assembled.routing_table.generation(), generation + 2);
    assert!(
        binding_hook(&fixture)
            .resolve(&mut Vec::new(), &mut ctx)
            .await
            .is_err()
    );
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests unavailable")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn canonical_binding_requires_authenticated_scope_and_unambiguous_recorded_source()
-> Result<()> {
    let fixture = fixture(false).await?;
    let identity = session(&fixture, "scoped", "fixture").await?;
    let mut ctx = PipelineContext::new(request(&identity, "scope-request", "coding")?);
    SessionContextHook::default().check(&mut ctx).await?;
    let normalized = ctx
        .extension::<RequestSessionContext>()
        .context("normalized context missing")?;
    assert!(
        fixture
            .assembled
            .gateway_requests
            .identify(&ctx, &normalized)
            .await?
            .is_some()
    );
    let mut foreign_request = request(&identity, "foreign-request", "coding")?;
    foreign_request.caller = CallerContext::new("public-key-id", "foreign-user");
    let mut foreign = PipelineContext::new(foreign_request);
    foreign.emit(ApiPrincipalEstablished {
        route_scope_id: "different-credential".into(),
    });
    SessionContextHook::default().check(&mut foreign).await?;
    let normalized_foreign = foreign
        .extension::<RequestSessionContext>()
        .context("normalized context missing")?;
    assert!(
        fixture
            .assembled
            .gateway_requests
            .identify(&foreign, &normalized_foreign)
            .await?
            .is_none()
    );
    session(&fixture, "scoped", "another-source").await?;
    assert!(
        fixture
            .assembled
            .gateway_requests
            .identify(&ctx, &normalized)
            .await?
            .is_none()
    );
    Ok(())
}

fn binding_hook(
    fixture: &Fixture,
) -> crate::gateway::observability::binding::CanonicalRequestBinding {
    crate::gateway::observability::binding::CanonicalRequestBinding::new(
        fixture.assembled.db.clone(),
        fixture.assembled.routing_table.clone(),
        fixture.assembled.policy_runtime.clone(),
    )
}

fn block_hook(fixture: &Fixture) -> crate::gateway::routing::blocks::runtime::BlockPolicyRouter {
    crate::gateway::routing::blocks::runtime::BlockPolicyRouter::new(
        fixture.assembled.db.clone(),
        fixture.assembled.routing_table.clone(),
        fixture.assembled.policy_runtime.clone(),
    )
}
