use super::*;
mod execution_guards;
mod judge_costs;
mod resource_coverage;
mod revisions;
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::Config;
use bitrouter_sdk::language_model::{
    ApiProtocol, GenerationParams, Message, PipelineRequest, Prompt, Role,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

use crate::acp_trajectory::{CanonicalStore, RecordingScope};
use crate::evolution::bandit::{Arm, BanditConfig};
use crate::evolution::control::{BlockRule, BlockStatus, EvolutionMode};
use crate::evolution::service::{CONTROL_KEY, CONTROL_KIND};
use crate::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use crate::session_identity::SessionContextHook;

struct Fixture {
    assembled: crate::assemble::Assembled,
    upstream: MockServer,
    _home: tempfile::TempDir,
}

async fn fixture(named: bool) -> Result<Fixture> {
    let upstream = MockServer::start().await;
    for model in ["strong", "cheap"] {
        Mock::given(method("POST")).and(path("/chat/completions"))
            .and(body_partial_json(json!({"model":model})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"fixture-response", "object":"chat.completion", "model":model,
                "choices":[{"index":0,"message":{"role":"assistant","content":"recorded response"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
            }))).mount(&upstream).await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model":"unavailable"})))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"error":{"message":"fixture unavailable"}})),
        )
        .mount(&upstream)
        .await;
    let home = tempfile::tempdir()?;
    let config_path = home.path().join("bitrouter.yaml");
    let mut config: Config = bitrouter_sdk::config::parse_with(
        &format!(
            r#"
server:
  skip_auth: true
database:
  url: "sqlite::memory:"
registry:
  inherit_defaults: false
policy:
  mode: frozen
providers:
  fixture:
    api_base: "{}"
    api_key: secret-not-for-evidence
    api_protocol:
      - "*": chat_completions
    models:
      - id: strong
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 4
      - id: cheap
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 1
          output_micro_usd_per_token: 2
      - id: unavailable
models:
  coding:
    endpoints:
      - provider: fixture
        service_id: strong
  candidate:
    endpoints:
      - provider: fixture
        service_id: cheap
  fallback:
    endpoints:
      - provider: fixture
        service_id: unavailable
      - provider: fixture
        service_id: cheap
presets:
  coding:
    model: "fixture:strong"
    system_prompt: "Use recorded requirements."
  candidate:
    model: "fixture:cheap"
    system_prompt: "Use recorded requirements."
    params:
      temperature: 0.2
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    if named {
        let mut lock = PolicyLock::default();
        for (name, default_tier) in [("coding", "strong"), ("candidate", "cheap")] {
            config
                .presets
                .get_mut(name)
                .context("fixture preset missing")?
                .policy = Some(name.into());
            lock.policies.insert(
                name.into(),
                PolicyDefinition {
                    tiers: BTreeMap::from([
                        ("strong".into(), "fixture:strong".into()),
                        ("cheap".into(), "fixture:cheap".into()),
                    ]),
                    default_tier: Some(default_tier.into()),
                    tool_use_tier: Some("strong".into()),
                    tool_safe_tiers: vec!["strong".into()],
                    ..PolicyDefinition::default()
                },
            );
        }
        tokio::fs::write(
            home.path().join("policy-lock.yaml"),
            deterministic_yaml(&lock)?,
        )
        .await?;
    }
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    Ok(Fixture {
        assembled,
        upstream,
        _home: home,
    })
}

async fn session(fixture: &Fixture, id: &str, source: &str) -> Result<SessionIdentity> {
    Ok(recording(fixture, id, source, true).await?.0)
}

async fn recording(
    fixture: &Fixture,
    id: &str,
    source: &str,
    acknowledge: bool,
) -> Result<(SessionIdentity, Arc<crate::acp_trajectory::Recorder>)> {
    let identity = SessionIdentity {
        owner: "local".into(),
        source: source.into(),
        native_session_id: id.into(),
    };
    let recorder = CanonicalStore::new(fixture.assembled.db.clone())
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: Some("controller".into()),
            route_scope_id: Some("local".into()),
        })
        .await?;
    if acknowledge {
        fixture
            .assembled
            .evolution
            .inventory()
            .register_capture(recorder.connection_id(), "local", "controller")
            .await?;
    }
    for (kind, method, call_id, payload) in [
        (
            CaptureKind::Request,
            "session/new",
            1,
            json!({"cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/new",
            1,
            json!({"result":{"sessionId":id}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            2,
            json!({"sessionId":id,"prompt":[{"type":"text","text":"Make a small change."}]}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(call_id),
                method: method.into(),
                payload,
            })
            .await?;
    }
    Ok((identity, recorder))
}

fn definition(selector: &str, challenger: &str) -> BlockDefinition {
    BlockDefinition {
        block_id: "coding-block".into(),
        source: "fixture".into(),
        rationale: "Compare complete compatible coding routes.".into(),
        rules: vec![BlockRule {
            selector: selector.into(),
            fingerprint: None,
            baseline_route: selector.into(),
            challenger_route: challenger.into(),
        }],
        independence_rationale: "One controlled block in this fixture.".into(),
        dependencies: BTreeMap::new(),
        measurement_contract: "fixture-measurement".into(),
        batch_sessions: 32,
        bandit: BanditConfig {
            initial_exposure_ppm: 500_000,
            ..BanditConfig::default()
        },
    }
}

fn request(identity: &SessionIdentity, id: &str, model: &str) -> Result<PipelineRequest> {
    let prompt = Prompt {
        model: model.into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "Make a small change.")],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new(model, CallerContext::local(), prompt);
    request.request_id = id.into();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    request
        .headers
        .insert("x-bitrouter-controller-id", "controller".parse()?);
    request.headers.insert(
        "x-bitrouter-acp-session-id",
        identity.native_session_id.parse()?,
    );
    Ok(request)
}

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
            crate::evolution::operator::EvolutionOperation::Restore {
                request: crate::evolution::control::restoration::RestoreRequest {
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
    with_tools.prompt.tools = serde_json::from_value(json!([{
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
    SessionContextHook::new(fixture.assembled.acp_runtime.clone())
        .check(&mut ctx)
        .await?;
    runtime.check(&mut ctx).await?;
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
    assert!(runtime.resolve(&mut Vec::new(), &mut ctx).await.is_err());
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
    SessionContextHook::new(fixture.assembled.acp_runtime.clone())
        .check(&mut ctx)
        .await?;
    let normalized = ctx
        .extension::<RequestSessionContext>()
        .context("normalized context missing")?;
    assert!(
        fixture
            .assembled
            .evolution
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
    SessionContextHook::new(fixture.assembled.acp_runtime.clone())
        .check(&mut foreign)
        .await?;
    let normalized_foreign = foreign
        .extension::<RequestSessionContext>()
        .context("normalized context missing")?;
    assert!(
        fixture
            .assembled
            .evolution
            .identify(&foreign, &normalized_foreign)
            .await?
            .is_none()
    );
    session(&fixture, "scoped", "another-source").await?;
    assert!(
        fixture
            .assembled
            .evolution
            .identify(&ctx, &normalized)
            .await?
            .is_none()
    );
    Ok(())
}
