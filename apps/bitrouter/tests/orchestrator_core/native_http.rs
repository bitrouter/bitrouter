//! Direct native/HTTP entry parity through one assembled production pipeline.
//! The control captures pipeline evidence; durable scheduling is covered by the
//! CoreSession fixtures. Both gateway and upstream use loopback HTTP sockets.

#[path = "native_http/costs.rs"]
mod costs;
#[path = "native_http/private_context.rs"]
mod private_context;
#[path = "native_http/protocol_matrix.rs"]
mod protocol_matrix;

fn generation(
    output: &bitrouter_sdk::language_model::types::PipelineOutput,
) -> anyhow::Result<bitrouter_ai::types::GenerateResult> {
    output
        .generation()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("generation fixture returned another operation"))
}

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum_test::TestServer;
use bitrouter::metering::entities::requests;
use bitrouter::metering::pricing::ChargeEvidence;
use bitrouter_ai::types::{
    GenerationParams, Message, Prompt, ReasoningEffort, ReasoningEffortSource, ResponseFormat,
    Role, Tool, ToolChoice,
};
use bitrouter_orchestrator::core::checkpoint::DurableHead;
use bitrouter_orchestrator::core::protocol::{
    Bind, Capabilities, HarnessManifest, Limits, OwnershipGrant,
};
use bitrouter_orchestrator::core::session::{CoreSession, RunStatus};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::extension::request_check::{Decision, Input};
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
    NativeProtocolValidation,
};
use bitrouter_sdk::language_model::native_accounting::NativeTokenCost;
use bitrouter_sdk::server::{AppState, build_router};
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Default)]
struct Capture {
    plans: Mutex<Vec<NativePlan>>,
    admissions: Mutex<Vec<(String, u32)>>,
    reports: Mutex<Vec<NativeAttemptReport>>,
}

#[async_trait]
impl NativeExecutionControl for Capture {
    async fn plan(&self, plan: NativePlan) -> bitrouter_sdk::error::Result<NativePlanAdmission> {
        let route_indices = plan
            .routes
            .iter()
            .enumerate()
            .map(|(index, _)| {
                u32::try_from(index).map_err(|_| {
                    bitrouter_sdk::error::BitrouterError::internal("fixture route overflow")
                })
            })
            .collect::<bitrouter_sdk::error::Result<Vec<_>>>()?;
        self.plans.lock().await.push(plan);
        Ok(NativePlanAdmission { route_indices })
    }

    async fn before_attempt(
        &self,
        request_id: &str,
        attempt_index: u32,
    ) -> bitrouter_sdk::error::Result<()> {
        self.admissions
            .lock()
            .await
            .push((request_id.to_owned(), attempt_index));
        Ok(())
    }

    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.reports.lock().await.push(report);
    }
}

fn schema() -> Value {
    json!({"type":"object", "properties":{"answer":{"type":"string"}},
        "required":["answer"], "additionalProperties":false})
}

fn native_prompt(text: &str) -> Prompt {
    Prompt {
        model: "bitrouter/parity".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, text)],
        tools: vec![Tool::Function {
            name: "inspect".into(),
            description: Some("Inspect evidence".into()),
            parameters: schema(),
            strict: Some(true),
            provider_metadata: Default::default(),
        }],
        params: GenerationParams {
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(128),
            reasoning_effort: Some(ReasoningEffort::High),
            reasoning_effort_source: ReasoningEffortSource::Caller,
            parallel_tool_calls: Some(false),
            store: Some(false),
            ..Default::default()
        },
        response_format: Some(ResponseFormat::JsonSchema {
            name: Some("answer".into()),
            description: None,
            strict: Some(true),
            schema: schema(),
        }),
        tool_choice: Some(ToolChoice::Auto),
        stream: false,
    }
}

// Independently authored inbound wire fixture, not rendered from native_prompt.
// https://developers.openai.com/api/reference/resources/responses/methods/create
fn http_prompt(text: &str) -> Value {
    json!({
        "model":"bitrouter/parity", "input":text, "stream":false, "store":false,
        "temperature":0.7, "top_p":0.9, "max_output_tokens":128,
        "reasoning":{"effort":"high"}, "parallel_tool_calls":false,
        "tools":[{"type":"function", "name":"inspect", "description":"Inspect evidence",
            "parameters":schema(), "strict":true}],
        "tool_choice":"auto",
        "text":{"format":{"type":"json_schema", "name":"answer", "schema":schema(), "strict":true}}
    })
}

struct Fixture {
    assembled: bitrouter::Assembled,
    gateway: TestServer,
    failing: MockServer,
    healthy: MockServer,
    checked: Arc<std::sync::Mutex<Vec<Input>>>,
    _runtime_home: tempfile::TempDir,
}

impl Fixture {
    async fn new(with_usage: bool, complete_capabilities: bool) -> Result<Self> {
        let failing = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(503).set_body_json(
                json!({"error":{"type":"server_error", "message":"fixture unavailable"}}),
            ))
            .mount(&failing)
            .await;
        let healthy = MockServer::start().await;
        let mut output = json!({
            "id":"resp_parity", "object":"response", "status":"completed", "model":"served-model",
            "output":[{"id":"msg_parity", "type":"message", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"{\"answer\":\"done\"}", "annotations":[]}]}]
        });
        if with_usage {
            output["usage"] = json!({"input_tokens":100,"output_tokens":5,"total_tokens":105,
                "input_tokens_details":{"cached_tokens":30,"cache_write_tokens":10},
                "output_tokens_details":{"reasoning_tokens":2}});
        }
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(output))
            .mount(&healthy)
            .await;
        let capabilities = if complete_capabilities {
            "[tools, reasoning, structured_outputs]"
        } else {
            "[reasoning]"
        };
        let source = format!(
            r#"
inherit_defaults: false
registry:
  enabled: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  failing:
    api_base: {}/v1
    api_key: fixture-failing
    models:
      - id: selected-model
        api_protocol: responses
        capabilities: {capabilities}
        pricing:
          input_micro_usd_per_token: 99
          output_micro_usd_per_token: 99
  healthy:
    api_base: {}/v1
    api_key: fixture-healthy
    models:
      - id: served-model
        api_protocol: responses
        capabilities: {capabilities}
        pricing:
          input_micro_usd_per_token: 2
          cache_read_micro_usd_per_token: 0.5
          cache_write_micro_usd_per_token: 3
          output_micro_usd_per_token: 4
          context_tiers:
            - above_input_tokens: 80
              input_micro_usd_per_token: 3
              output_micro_usd_per_token: 6
models:
  resilient:
    endpoints:
      - {{provider: failing, service_id: selected-model}}
      - {{provider: healthy, service_id: served-model}}
checkers:
  parity:
    native:
      revision: fixture-v1
routers:
  parity:
    selection:
      kind: model
      model: resilient
    defaults:
      system_prompt: required-router-instruction
      params:
        temperature: 0.2
        reasoning_effort: low
        max_tokens: 64
    checks:
      request:
        - checker: parity
          timeout_ms: 5000
"#,
            failing.uri(),
            healthy.uri()
        );
        let config = bitrouter_sdk::config::parse_with(&source, |_| None)?;
        let runtime_home = tempfile::tempdir()?;
        let config_path = runtime_home.path().join("bitrouter.yaml");
        std::fs::write(&config_path, source)?;
        let checked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = checked.clone();
        let assembled =
            bitrouter::assemble::build_app_with_extensions(&config, Some(&config_path), |api| {
                api.request_check(
                    "parity",
                    "fixture-v1",
                    Arc::new(move |input| {
                        let denied = input
                            .content
                            .iter()
                            .any(|part| part.text.as_deref() == Some("deny-this-task"));
                        match observed.lock() {
                            Ok(mut rows) => rows.push(input.clone()),
                            Err(_) => {
                                return Decision::Deny {
                                    reason_code: "fixture.capture_unavailable".into(),
                                };
                            }
                        }
                        if denied {
                            Decision::Deny {
                                reason_code: "fixture.denied".into(),
                            }
                        } else {
                            Decision::Allow
                        }
                    }),
                )?;
                Ok(())
            })
            .await?;
        let gateway = TestServer::builder()
            .http_transport()
            .try_build(build_router(AppState {
                language_model: assembled
                    .app
                    .language_model()
                    .context("missing pipeline")?
                    .clone(),
                mcp: assembled.app.mcp().cloned(),
                skip_auth: assembled.app.skip_auth(),
                metrics_renderer: assembled.app.metrics_renderer().cloned(),
                prompt_transforms: assembled.app.prompt_transforms().to_vec(),
            }))?;
        Ok(Self {
            assembled,
            gateway,
            failing,
            healthy,
            checked,
            _runtime_home: runtime_home,
        })
    }

    fn assert_checks_match(&self) -> Result<()> {
        let checked = self
            .checked
            .lock()
            .map_err(|_| anyhow::anyhow!("checker capture poisoned"))?;
        assert_eq!(checked.len(), 2, "one bound request check per entry");
        assert_eq!(checked[0], checked[1]);
        assert!(
            checked[0]
                .content
                .iter()
                .any(|part| part.text.as_deref() == Some("required-router-instruction"))
        );
        Ok(())
    }
}

#[tokio::test]
async fn native_and_http_preserve_constraints_fallback_and_accounting() -> Result<()> {
    for (with_usage, complete_capabilities) in [(true, true), (false, true), (true, false)] {
        let fixture = Fixture::new(with_usage, complete_capabilities).await?;
        let capture = Arc::new(Capture::default());
        let native = fixture
            .assembled
            .app
            .execute_native_controlled(
                native_prompt("same-task"),
                CallerContext::local(),
                capture.clone(),
            )
            .await?;
        let response = fixture
            .gateway
            .post("/v1/responses")
            .json(&http_prompt("same-task"))
            .await;
        assert_eq!(response.status_code().as_u16(), 200, "{}", response.text());
        let http: Value = response.json();
        let http_request_id = response
            .header("x-bitrouter-request-id")
            .to_str()?
            .to_owned();
        assert_ne!(http_request_id, native.request_id);
        assert_eq!(http["model"], "served-model");
        fixture.assert_checks_match()?;

        let plans = capture.plans.lock().await;
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.request_id, native.request_id);
        assert_eq!(plan.original_model, "bitrouter/parity");
        assert_eq!(plan.effective_model, "resilient");
        let binding = plan.router.as_ref().context("missing bound router")?;
        assert_eq!(binding.router_id, "parity");
        assert!(!binding.binding_digest.is_empty());
        assert_eq!(plan.effort_source, ReasoningEffortSource::Caller);
        assert_eq!(
            plan.prompt.params.reasoning_effort,
            Some(ReasoningEffort::High)
        );
        assert_eq!(plan.prompt.params.max_tokens, Some(128));
        assert_eq!(
            plan.routes
                .iter()
                .map(|route| (route.provider.as_str(), route.model.as_str()))
                .collect::<Vec<_>>(),
            [("failing", "selected-model"), ("healthy", "served-model")]
        );
        assert!(
            plan.routes
                .iter()
                .all(|route| route.protocol_validation == NativeProtocolValidation::Compatible)
        );
        assert_eq!(
            *capture.admissions.lock().await,
            [
                (native.request_id.clone(), 0),
                (native.request_id.clone(), 1)
            ]
        );

        let reports = capture.reports.lock().await;
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].attempt_index, 0);
        assert_eq!(reports[0].route, plan.routes[0]);
        assert!(reports[0].error.is_some());
        assert!(reports[0].result.is_none());
        assert!(matches!(
            reports[0].token_cost,
            NativeTokenCost::Unknown { .. }
        ));
        assert_eq!(reports[1].attempt_index, 1);
        let served = &reports[1];
        assert_eq!(served.route, plan.routes[1]);
        assert_eq!(served.request_id, native.request_id);
        assert_eq!(served.actual_provider.as_deref(), Some("healthy"));
        assert_eq!(served.actual_model.as_deref(), Some("served-model"));
        assert_eq!(served.result.as_ref(), Some(&generation(&native.result)?));

        for (upstream, model) in [
            (&fixture.failing, "selected-model"),
            (&fixture.healthy, "served-model"),
        ] {
            let requests = upstream
                .received_requests()
                .await
                .context("missing upstream capture")?;
            assert_eq!(
                requests.len(),
                2,
                "each entry makes exactly one request per route"
            );
            let native_body: Value = serde_json::from_slice(&requests[0].body)?;
            let http_body: Value = serde_json::from_slice(&requests[1].body)?;
            assert_eq!(
                native_body, http_body,
                "complete provider request must match"
            );
            assert_eq!(native_body["model"], model);
            assert_eq!(native_body["temperature"], 0.7);
            assert_eq!(native_body["top_p"], 0.9);
            assert_eq!(native_body["max_output_tokens"], 128);
            assert_eq!(native_body["reasoning"]["effort"], "high");
            assert_eq!(native_body["parallel_tool_calls"], false);
            assert_eq!(native_body["store"], false);
            assert_eq!(native_body["tools"][0]["parameters"], schema());
            assert_eq!(native_body["tools"][0]["strict"], true);
            assert_eq!(native_body["tools"], http_prompt("same-task")["tools"]);
            assert_eq!(native_body["tool_choice"], "auto");
            assert_eq!(native_body["text"]["format"]["schema"], schema());
            assert_eq!(native_body["text"], http_prompt("same-task")["text"]);
            assert!(
                native_body
                    .to_string()
                    .contains("required-router-instruction")
            );
            assert!(native_body.to_string().contains("same-task"));
        }
        fixture
            .assembled
            .app
            .language_model()
            .context("missing pipeline")?
            .drain_required_pending_settlements()
            .await?;
        let rows = requests::Entity::find().all(&fixture.assembled.db).await?;
        assert_eq!(
            rows.len(),
            2,
            "one settlement per logical request, not per fallback attempt"
        );
        let native_row = rows
            .iter()
            .find(|row| row.request_id == native.request_id)
            .context("missing native settlement")?;
        let http_row = rows
            .iter()
            .find(|row| row.request_id == http_request_id)
            .context("missing HTTP settlement")?;
        assert_eq!(
            native_row.charge_evidence_json,
            http_row.charge_evidence_json
        );
        assert_eq!(native_row.raw_usage_json, http_row.raw_usage_json);
        assert_eq!(native_row.binding_digest, http_row.binding_digest);
        for row in &rows {
            assert_eq!(row.provider_id, "healthy");
            assert_eq!(row.model_id, "served-model");
            assert_eq!(row.original_selector.as_deref(), Some("bitrouter/parity"));
            assert_eq!(row.router_id.as_deref(), Some("parity"));
            assert_eq!(
                row.binding_digest.as_deref(),
                Some(binding.binding_digest.as_str())
            );
            assert_eq!(row.user_id, "local");
            assert_eq!(row.api_key_id, "local");
            assert_eq!(row.streamed, 0);
            assert!(row.error.is_none());
            if with_usage {
                assert_eq!(row.estimated_charge_micro_usd, 255);
                assert_eq!(row.charge_status, "computed");
                assert_eq!(row.usage_origin, "provider_reported");
                let evidence: ChargeEvidence = serde_json::from_str(
                    row.charge_evidence_json
                        .as_deref()
                        .context("missing evidence")?,
                )?;
                let NativeTokenCost::ConfiguredEstimate {
                    micro_usd,
                    pricing_version,
                    normalized_usage,
                    pricing_provider,
                    pricing_model,
                    ..
                } = &served.token_cost
                else {
                    anyhow::bail!("missing configured native estimate");
                };
                assert_eq!(*micro_usd, 255);
                assert_eq!(pricing_provider, "healthy");
                assert_eq!(pricing_model, "served-model");
                assert_eq!(&evidence.pricing_version, pricing_version);
                assert_eq!(&evidence.normalized_usage, normalized_usage);
                assert_eq!(served.cache.read_tokens, Some(30));
                assert_eq!(served.cache.write_tokens, Some(10));
                assert_eq!(http["usage"]["input_tokens"], 100);
                assert_eq!(http["usage"]["output_tokens"], 5);
            } else {
                assert_eq!(row.charge_status, "unknown");
                assert_eq!(row.usage_origin, "unknown");
                assert!(matches!(served.token_cost, NativeTokenCost::Unknown { .. }));
                assert!(
                    served
                        .result
                        .as_ref()
                        .is_some_and(|result| result.usage.is_none())
                );
                assert_eq!(served.cache.read_tokens, None);
                assert_eq!(served.cache.write_tokens, None);
                assert!(http.get("usage").is_none_or(Value::is_null));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_and_http_share_bound_denial_before_any_dispatch() -> Result<()> {
    let fixture = Fixture::new(true, true).await?;
    let capture = Arc::new(Capture::default());
    let native = fixture
        .assembled
        .app
        .execute_native_controlled(
            native_prompt("deny-this-task"),
            CallerContext::local(),
            capture.clone(),
        )
        .await;
    let error = native.err().context("denied native request succeeded")?;
    assert_eq!(error.error_code(), "permission_denied");
    assert!(
        matches!(&error, bitrouter_sdk::error::BitrouterError::Forbidden(message)
        if message == "request denied by configured checker")
    );
    let response = fixture
        .gateway
        .post("/v1/responses")
        .json(&http_prompt("deny-this-task"))
        .await;
    assert_eq!(response.status_code().as_u16(), 403, "{}", response.text());
    assert_eq!(
        response.json::<Value>()["error"]["message"],
        error.public_message()
    );
    assert_eq!(
        response.json::<Value>()["error"]["code"],
        error.error_code()
    );
    fixture.assert_checks_match()?;
    assert!(capture.plans.lock().await.is_empty());
    assert!(capture.admissions.lock().await.is_empty());
    assert!(capture.reports.lock().await.is_empty());
    assert!(
        fixture
            .failing
            .received_requests()
            .await
            .context("missing failed capture")?
            .is_empty()
    );
    assert!(
        fixture
            .healthy
            .received_requests()
            .await
            .context("missing healthy capture")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn core_retains_unknown_capabilities_without_inventing_unsupported() -> Result<()> {
    let fixture = Fixture::new(true, false).await?;
    let grant = OwnershipGrant {
        session_id: "parity-session".into(),
        harness_id: "parity-harness".into(),
        core_instance_id: "parity-core".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(super::Harness {
        grant: grant.clone(),
        store: Mutex::new(super::Store::default()),
    });
    let session = CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest: HarnessManifest {
                tool_manifest_digest: HarnessManifest::digest(&[])?,
                tools: Vec::new(),
                workspace_id: "parity-workspace".into(),
                workspace_revision: None,
                permission_revision: 1,
                max_tool_output_bytes: 8192,
                artifact_quota_bytes: 1024 * 1024,
                max_artifact_chunk_bytes: 8192,
                required_features: Vec::new(),
            },
            limits: Limits::default(),
        },
        &Capabilities {
            version: 1,
            core_instance_id: "parity-core".into(),
            operations: Vec::new(),
            transports: vec!["in_process".into()],
            unsupported_features: Vec::new(),
            limits: Limits::default(),
            max_sessions: 16,
            max_host_model_attempts: 16,
        },
        Arc::new(fixture.assembled.app),
        CallerContext::local(),
        harness,
    )
    .await?;
    let mut input = super::input("same-task");
    input.model = "resilient".into();
    input.effort = Some("high".into());
    session
        .start("start", session.head().await.state_revision, input)
        .await?;
    let state = session.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let turn = state.root_turn().context("missing root turn")?;
    assert_eq!(
        turn.steps.len(),
        1,
        "unknown metadata must not trigger reconstruction"
    );
    let step = &turn.steps[0];
    assert!(step.reconstructed_from.is_none());
    assert_eq!(step.attempts.len(), 2);
    let run = state.run.as_ref().context("missing run")?;
    let accounting = run
        .token_accounting
        .as_ref()
        .context("missing accounting")?;
    assert_eq!(accounting.known_attempts, 1);
    assert_eq!(accounting.unknown_attempts, 1);
    assert_eq!(
        accounting.complete_estimate_micro_usd(run.model_attempts),
        None
    );
    let decision = step.decision.as_ref().context("missing decision")?;
    for route in &decision.routes {
        assert!(route.rejection_reasons.is_empty());
        assert!(
            route
                .unverified_constraints
                .contains(&"required_capability_unknown".into())
        );
    }
    for (upstream, model) in [
        (&fixture.failing, "selected-model"),
        (&fixture.healthy, "served-model"),
    ] {
        let requests = upstream
            .received_requests()
            .await
            .context("missing capture")?;
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body)?;
        assert_eq!(body["model"], model);
        assert!(
            body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        );
        assert_eq!(body["reasoning"]["effort"], "high");
    }
    Ok(())
}
