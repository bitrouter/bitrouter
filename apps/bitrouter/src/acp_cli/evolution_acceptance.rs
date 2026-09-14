//! Explicit acceptance with the pinned ACP adapters and their real workers.
//! Model responses are served locally; no developer credentials are inherited.
//! The session host is the same launch/capture boundary used by the coding TUI.
//!
//! Adapter contracts: https://github.com/agentclientprotocol/codex-acp and
//! https://github.com/agentclientprotocol/claude-agent-acp .

use super::*;
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

use anyhow::ensure;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

use crate::acp_trajectory::checkpoint::types::CriterionScore;
use crate::acp_trajectory::{CanonicalStore, SessionIdentity};
use crate::evolution::control::{BlockDefinition, BlockRule, EvolutionMode};
use crate::evolution::evidence::{EvidenceKind, EvidencePacket};
use crate::evolution::rubric::{
    Applicability, RUBRIC_VERSION, RubricEvaluation, RubricItem, library,
};
use crate::evolution::runtime::EvolutionRuntime;
use crate::evolution::scheduler::EvolutionScheduler;

mod publication;

struct Fixture {
    selector: &'static str,
    runtime: EvolutionRuntime,
    canonical: CanonicalStore,
    config: Config,
    source: ConfigSource,
    pipeline: Arc<bitrouter_sdk::language_model::Pipeline>,
    ingress: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    upstream: MockServer,
    home: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Deliberately fixed fixture labels, not an accuracy assessment of a real judge.
fn fixture_judge(body: &Value) -> Result<String> {
    let text = body["messages"]
        .as_array()
        .context("judge messages missing")?
        .last()
        .and_then(|m| m["content"].as_str())
        .context("judge evidence missing")?;
    let input: Value = serde_json::from_str(text)?;
    let packet: EvidencePacket = serde_json::from_value(input["evidence"].clone())?;
    let citations = packet
        .items
        .iter()
        .filter(|i| {
            matches!(
                i.kind,
                EvidenceKind::UserMessage | EvidenceKind::AgentMessage
            )
        })
        .map(|i| i.citation.clone())
        .collect::<Vec<_>>();
    ensure!(!citations.is_empty(), "no recorded fixture messages");
    let quality = if packet.items.iter().any(|item| {
        item.kind == EvidenceKind::UserMessage
            && item.content.to_string().contains("ACP_ACCEPTANCE_REJECTED")
    }) {
        50_000
    } else {
        950_000
    };
    Ok(serde_json::to_string(&RubricEvaluation {
        rubric_version: RUBRIC_VERSION.into(),
        items: library()
            .into_iter()
            .map(|template| RubricItem {
                criterion_id: template.id.into(),
                applicability: if template.mandatory {
                    Applicability::Applicable
                } else {
                    Applicability::NotApplicable
                },
                selection_reason:
                    "Controlled task requests a literal reply without tools, tests, review or a PR."
                        .into(),
                score: if template.mandatory {
                    CriterionScore::Scored { value_ppm: quality }
                } else {
                    CriterionScore::NotApplicable
                },
                evidence: citations.clone(),
                explanation:
                    "Fixed acceptance label over the recorded reply; not judge calibration.".into(),
            })
            .collect(),
        diagnostics: vec![],
        severe_violation: false,
        violation_evidence: vec![],
        summary: "Controlled reply fixture.".into(),
    })?)
}

fn response(request: &Request) -> Result<ResponseTemplate> {
    let body: Value = serde_json::from_slice(&request.body)?;
    let model = body["model"].as_str().context("model missing")?;
    let text = if model == "judge" {
        fixture_judge(&body)?
    } else {
        "ACP_ACCEPTANCE_OK".into()
    };
    let usage = json!({"prompt_tokens":20,"completion_tokens":10,"total_tokens":30});
    if body["stream"].as_bool() == Some(true) {
        // OpenAI-compatible chat streaming is translated by the actual gateway:
        // https://platform.openai.com/docs/api-reference/chat/streaming .
        let chunks = [
            json!({"id":"fixture-chat","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
            json!({"id":"fixture-chat","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
        ];
        let mut stream = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        stream.push_str("data: [DONE]\n\n");
        Ok(ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stream))
    } else {
        Ok(ResponseTemplate::new(200).set_body_json(json!({"id":"fixture-chat","object":"chat.completion","created":0,"model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":usage})))
    }
}

async fn fixture(agent: &str, adapter_variable: &str, worker_variable: &str) -> Result<Fixture> {
    fixture_with_response(agent, adapter_variable, worker_variable, response).await
}

async fn fixture_with_response(
    agent: &str,
    adapter_variable: &str,
    worker_variable: &str,
    responder: fn(&Request) -> Result<ResponseTemplate>,
) -> Result<Fixture> {
    // A known model alias avoids the worker's model-metadata warning, which is
    // sent as ordinary assistant text before the first HTTP call. Such text
    // deliberately still prevents late trial enrollment.
    let selector = if agent == "codex-acp" {
        "gpt-5.2"
    } else {
        "coding"
    };
    ensure!(
        std::env::var("BITROUTER_API_KEY")
            .unwrap_or_default()
            .is_empty(),
        "acceptance requires an empty BITROUTER_API_KEY"
    );
    let adapter =
        PathBuf::from(std::env::var(adapter_variable).with_context(|| {
            format!("set {adapter_variable} to the pinned adapter entry point")
        })?);
    let package_file = adapter
        .parent()
        .and_then(Path::parent)
        .context("adapter package path missing")?
        .join("package.json");
    let package: Value = serde_json::from_slice(&std::fs::read(package_file)?)?;
    let harness = crate::harness::by_id(agent).context("unknown harness")?;
    let (name, version) = harness
        .maintained_adapter_identity()
        .context("adapter is not pinned")?;
    ensure!(
        package["name"] == name && package["version"] == version,
        "adapter version differs from the maintained pin"
    );
    let worker = std::env::var(worker_variable)
        .with_context(|| format!("set {worker_variable} to the isolated worker executable"))?;
    let home = tempfile::tempdir()?;
    for name in ["workspace", "codex", "tmp"] {
        std::fs::create_dir(home.path().join(name))?;
    }
    let wrapper = home.path().join("adapter-wrapper");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nexec \"$BITROUTER_TEST_NODE\" \"$BITROUTER_TEST_ADAPTER\"\n",
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &Request| {
            responder(request).unwrap_or_else(|error| {
                ResponseTemplate::new(500).set_body_string(format!("fixture response: {error:#}"))
            })
        })
        .mount(&upstream)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let listen = listener.local_addr()?.to_string();
    let mut config: Config = bitrouter_sdk::config::parse_with(
        &format!(
            r#"
server:
  listen: "{listen}"
  skip_auth: true
database:
  url: "sqlite://{}?mode=rwc"
registry:
  inherit_defaults: false
acp_recording:
  enabled: true
providers:
  fixture:
    api_base: "{}"
    api_key: fixture-only
    api_protocol:
      - "*": chat_completions
    models:
      - id: strong
        capabilities: [tools, reasoning]
        pricing: {{ input_micro_usd_per_token: 10, output_micro_usd_per_token: 10 }}
      - id: cheap
        capabilities: [tools, reasoning]
        pricing: {{ input_micro_usd_per_token: 1, output_micro_usd_per_token: 1 }}
      - id: judge
        pricing: {{ input_micro_usd_per_token: 1, output_micro_usd_per_token: 1 }}
models:
  {selector}:
    endpoints: [{{ provider: fixture, service_id: strong }}]
  candidate:
    endpoints: [{{ provider: fixture, service_id: cheap }}]
  judge:
    endpoints: [{{ provider: fixture, service_id: judge }}]
"#,
            home.path().join("bitrouter.db").display(),
            upstream.uri()
        ),
        |_| None,
    )?;
    let mut env = HashMap::from([
        ("PATH".into(), std::env::var("PATH")?),
        ("HOME".into(), home.path().display().to_string()),
        (
            "CODEX_HOME".into(),
            home.path().join("codex").display().to_string(),
        ),
        (
            "TMPDIR".into(),
            home.path().join("tmp").display().to_string(),
        ),
        (
            "BITROUTER_TEST_NODE".into(),
            std::env::var("BITROUTER_TEST_NODE").unwrap_or_else(|_| "node".into()),
        ),
        (
            "BITROUTER_TEST_ADAPTER".into(),
            adapter.display().to_string(),
        ),
        ("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()),
        ("no_proxy".into(), "localhost,127.0.0.1,::1".into()),
        ("DISABLE_TELEMETRY".into(), "1".into()),
    ]);
    env.insert(
        if agent == "codex-acp" {
            "CODEX_PATH"
        } else {
            "CLAUDE_CODE_EXECUTABLE"
        }
        .into(),
        worker,
    );
    config.agents.insert(
        agent.into(),
        bitrouter_sdk::acp::transport::AcpAgentConfig {
            name: agent.into(),
            transport: AcpTransport::Stdio {
                command: wrapper.display().to_string(),
                args: vec![format!("{name}@{version}")],
                env,
            },
        },
    );
    let source = ConfigSource::Default {
        home: home.path().to_path_buf(),
    };
    let assembled =
        crate::assemble::build_app_with_path(&config, Some(&home.path().join("bitrouter.yaml")))
            .await?;
    let runtime = assembled.evolution.clone();
    let canonical = CanonicalStore::new(assembled.db.clone());
    let app = Arc::new(assembled.app);
    let pipeline = app.language_model().cloned().context("pipeline missing")?;
    let ingress = Arc::new(Mutex::new(Vec::new()));
    let seen = ingress.clone();
    let router = build_router(AppState {
        language_model: pipeline.clone(),
        mcp: app.mcp().cloned(),
        skip_auth: app.skip_auth(),
        metrics_renderer: app.metrics_renderer().cloned(),
        prompt_transforms: app.prompt_transforms().to_vec(),
    })
    .layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let seen = seen.clone();
            async move {
                if request.method() == axum::http::Method::POST
                    && ["/responses", "/messages", "/chat/completions"]
                        .iter()
                        .any(|end| request.uri().path().ends_with(end))
                {
                    let headers = [
                        "x-bitrouter-controller-id",
                        "x-bitrouter-harness",
                        "x-bitrouter-acp-session-id",
                        "session-id",
                        "thread-id",
                        "x-claude-code-session-id",
                    ]
                    .into_iter()
                    .filter_map(|name| {
                        request
                            .headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(|v| (name.into(), v.into()))
                    })
                    .collect();
                    match seen.lock() {
                        Ok(mut seen) => seen.push(headers),
                        Err(poisoned) => poisoned.into_inner().push(headers),
                    }
                }
                next.run(request).await
            }
        },
    ));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let socket = crate::daemon::socket_path_for(&source, &config);
    let control = tokio::spawn({
        let runtime = runtime.clone();
        let socket = socket.clone();
        async move {
            let _ = crate::daemon::run_control_socket_with_acp_runtime(
                socket,
                app,
                listen,
                Arc::new(crate::daemon::NoopReloader),
                Arc::new(crate::daemon::NoopObserveStatus { compiled_in: false }),
                crate::daemon::AcpControlPlane {
                    runtime: assembled.acp_runtime,
                    metering: crate::metering::MeteringStore::new(assembled.db),
                    inventory: Some(runtime.inventory()),
                    evolution: Some(runtime),
                },
            )
            .await;
        }
    });
    let fixture = Fixture {
        selector,
        runtime,
        canonical,
        config,
        source,
        pipeline,
        ingress,
        tasks: vec![server, control],
        upstream,
        home,
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(fixture)
}

async fn configure(fixture: &Fixture, agent: &str) -> Result<()> {
    fixture
        .runtime
        .register(
            "local",
            BlockDefinition {
                block_id: "acceptance".into(),
                source: agent.into(),
                rationale: "Controlled integrated path".into(),
                rules: vec![BlockRule {
                    selector: fixture.selector.into(),
                    fingerprint: None,
                    baseline_route: fixture.selector.into(),
                    challenger_route: "candidate".into(),
                }],
                independence_rationale: "One fixture block".into(),
                dependencies: BTreeMap::new(),
                measurement_contract: crate::evolution::scoring::measurement_contract(
                    crate::acp_trajectory::checkpoint::types::AssessmentSource::Agentic,
                    "checkpoint-judge:judge",
                    crate::evolution::judge::JUDGE_VERSION,
                )?,
                batch_sessions: 4,
                bandit: crate::evolution::bandit::BanditConfig {
                    initial_exposure_ppm: 500_000,
                    ..Default::default()
                },
            },
        )
        .await?;
    let service = fixture.runtime.service("local")?;
    service
        .set_mode(EvolutionMode::Automatic, Some("judge".into()))
        .await?;
    Ok(())
}

async fn launch(fixture: &Fixture, agent: &str) -> Result<SessionHandle> {
    let host = SessionHost::prepare(
        SpawnContext {
            source: &fixture.source,
            config: fixture.config.clone(),
            agent_id: agent,
            options: LaunchOptions {
                strip_inherited_env: std::env::vars_os()
                    .filter_map(|(name, _)| name.into_string().ok())
                    .collect(),
                turn_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            },
            routing: RoutingOptions {
                no_start: true,
                model: Some(fixture.selector.into()),
                ..Default::default()
            },
        },
        false,
    )
    .await?;
    host.open(
        &SessionSelection::New,
        fixture.home.path().join("workspace"),
    )
    .await
}

async fn run(agent: &str, adapter_variable: &str, worker_variable: &str) -> Result<()> {
    let fixture = fixture(agent, adapter_variable, worker_variable).await?;
    configure(&fixture, agent).await?;
    let mut handle = launch(&fixture, agent).await?;
    let result = exercise_session(&fixture, &handle).await;
    let clean = handle.shutdown().await;
    result?;
    ensure!(clean, "adapter cleanup was not confirmed");
    verify_closed_session(&fixture, &handle).await?;
    Ok(())
}

async fn verify_judge_costs(
    fixture: &Fixture,
    minimum_calls: usize,
) -> Result<crate::evolution::costs::report::CostSummary> {
    use crate::evolution::operator::{EvolutionOperation, EvolutionReport};
    let EvolutionReport::Status(status) = fixture
        .runtime
        .operate("local", EvolutionOperation::Status)
        .await?
    else {
        anyhow::bail!("expected evolution status");
    };
    let mut calls = 0;
    for request in fixture
        .upstream
        .received_requests()
        .await
        .context("HTTP observations missing")?
    {
        let body: Value = serde_json::from_slice(&request.body)?;
        calls += usize::from(body["model"] == "judge");
    }
    ensure!(
        calls >= minimum_calls
            && status.judge_costs.summary.requests == calls
            && status.judge_costs.summary.total_cost_micro_usd == Some(30 * calls as u64),
        "judge ledger must match actual HTTP calls exactly once, including any superseded prefix"
    );
    Ok(status.judge_costs.summary)
}

async fn verify_closed_session(fixture: &Fixture, handle: &SessionHandle) -> Result<()> {
    use crate::acp_trajectory::checkpoint::types::RESOURCE_MEMBERSHIP_VERSION;
    use std::collections::BTreeSet;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: handle.agent_id.clone(),
        native_session_id: handle.session_id.clone(),
    };
    let scheduler = EvolutionScheduler::new(fixture.runtime.clone());
    let (effective, executions) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let report = scheduler.tick(&fixture.pipeline).await?;
            ensure!(
                report.errors.values().all(|reason| matches!(
                    reason.as_str(),
                    "checkpoint_discovery_retry_required"
                        | "resource_refresh_retry_required"
                        | "learning_or_route_validation_retry_required"
                )),
                "closed-session scheduler errors: {:?}",
                report.errors
            );
            let effective = fixture.canonical.effective_assessment(&identity).await?;
            let executions = fixture.runtime.executions(&identity).await?;
            if report.errors.is_empty()
                && !effective.stale
                && effective.assessment.is_some()
                && executions.iter().all(|execution| {
                    execution
                        .settlement
                        .as_ref()
                        .is_some_and(|settled| settled.outcome.is_some())
                })
            {
                return Ok::<_, anyhow::Error>((effective, executions));
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("closed-session accounting did not converge")??;
    ensure!(
        !effective.source_capture_states.is_empty()
            && effective
                .source_capture_states
                .values()
                .all(|state| state == "complete"),
        "capture did not close cleanly: {:?}",
        effective.source_capture_states
    );
    let resource = effective
        .resource
        .context("closed-session resources missing")?;
    ensure!(
        resource.membership_version.as_deref() == Some(RESOURCE_MEMBERSHIP_VERSION),
        "resource membership contract is stale"
    );
    let actual: BTreeSet<_> = executions
        .iter()
        .map(|execution| execution.request_id.as_str())
        .collect();
    let ingress = fixture
        .ingress
        .lock()
        .map_err(|_| anyhow::anyhow!("ingress lock poisoned"))?
        .clone();
    ensure!(
        actual.len() == ingress.len(),
        "a gateway request escaped canonical accounting before worker exit: {ingress:?}"
    );
    let included: BTreeSet<_> = resource
        .requests
        .iter()
        .map(|request| request.request_id.as_str())
        .collect();
    ensure!(
        actual == included && resource.unassigned_request_ids.is_empty(),
        "closed-session resources omitted an actual gateway request"
    );
    let mut known = 0_u64;
    let mut unpriced = 0;
    for execution in &executions {
        match execution
            .settlement
            .as_ref()
            .and_then(|settled| settled.total_cost_micro_usd)
        {
            Some(cost) => {
                known = known
                    .checked_add(cost)
                    .context("closed-session cost overflow")?
            }
            None => unpriced += 1,
        }
    }
    ensure!(
        resource.known_cost_micro_usd == i64::try_from(known)?
            && resource.unpriced_requests == unpriced,
        "closed-session cost differs from its full execution inventory"
    );
    ensure!(
        resource.metering_complete == (unpriced == 0),
        "closed-session completeness disagrees with actual settlements"
    );
    let learning = fixture
        .runtime
        .service("local")?
        .learning_status("acceptance")
        .await?;
    ensure!(
        learning.observations.sessions.len() == 1,
        "closing must not create another independent sample"
    );
    let observation = learning
        .observations
        .sessions
        .values()
        .next()
        .context("closed-session observation missing")?;
    ensure!(
        observation.quality.is_some()
            && observation.total_cost_micro_usd == (unpriced == 0).then_some(known),
        "closed-session learning must use the full known cost or explicit unknown cost"
    );
    let before = verify_judge_costs(fixture, 2).await?;
    let retry = scheduler.tick(&fixture.pipeline).await?;
    ensure!(
        retry.errors.is_empty() && retry.jobs_completed == 0 && retry.checkpoints_created == 0,
        "unchanged closed checkpoint triggered repeated evaluation"
    );
    ensure!(
        verify_judge_costs(fixture, 2).await? == before,
        "resource-only refresh repeated a judge charge"
    );
    eprintln!(
        "{}: closed; {} actual requests; all included; known coding cost {known} micro-USD; {unpriced} unpriced; one effective session; judge cost {:?} micro-USD",
        handle.agent_id,
        executions.len(),
        before.total_cost_micro_usd
    );
    Ok(())
}

async fn exercise_session(fixture: &Fixture, handle: &SessionHandle) -> Result<()> {
    let identity = SessionIdentity {
        owner: "local".into(),
        source: handle.agent_id.clone(),
        native_session_id: handle.session_id.clone(),
    };
    let service = fixture.runtime.service("local")?;
    let mut previous_cost = 0;
    let mut previous_checkpoint = None;
    let mut assignments = None;
    for turn in 1..=2 {
        let response = handle.client.prompt(&handle.session_id, "Reply exactly ACP_ACCEPTANCE_OK. Do not use tools, run tests, request a review or create a PR.").await?;
        ensure!(
            response.stop_reason == agent_client_protocol::schema::v1::StopReason::EndTurn,
            "adapter did not complete the fixture turn"
        );
        let enrollment = service
            .enrollment(&identity)
            .await?
            .context("actual harness session was not enrolled")?;
        ensure!(
            enrollment.assignments.len() == 1,
            "trial assignment missing: {enrollment:?}"
        );
        let current_assignment = serde_json::to_value(&enrollment.assignments)?;
        if let Some(previous) = assignments.as_ref() {
            ensure!(
                previous == &current_assignment,
                "continued session was reassigned"
            );
        }
        assignments = Some(current_assignment);
        // Real adapters may append metadata just after prompt completion. Run
        // the daemon's next pass when the canonical head fence asks for retry;
        // all other operational failures remain errors in this acceptance.
        let effective = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let report = EvolutionScheduler::new(fixture.runtime.clone())
                    .tick(&fixture.pipeline)
                    .await?;
                ensure!(
                    report
                        .errors
                        .values()
                        .all(|reason| reason == "checkpoint_discovery_retry_required"),
                    "scheduler errors: {:?}",
                    report.errors
                );
                let effective = fixture.canonical.effective_assessment(&identity).await?;
                if report.errors.is_empty() && !effective.stale && effective.assessment.is_some() {
                    return Ok::<_, anyhow::Error>(effective);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("automatic assessment did not converge")??;
        let checkpoint = effective
            .checkpoint
            .context("effective checkpoint missing")?;
        let resource = effective
            .resource
            .context("effective resource observation missing")?;
        ensure!(checkpoint.watermark > 0, "session was not captured");
        let executions = fixture.runtime.executions(&identity).await?;
        let inbound = fixture
            .ingress
            .lock()
            .map_err(|_| anyhow::anyhow!("ingress lock poisoned"))?
            .clone();
        ensure!(
            !inbound.is_empty() && executions.len() == inbound.len(),
            "canonical execution coverage differs from actual ingress: {} executions / {} requests; headers: {inbound:?}",
            executions.len(),
            inbound.len()
        );
        ensure!(
            previous_checkpoint.as_ref() != Some(&checkpoint.checkpoint_id),
            "continued session reused its previous checkpoint"
        );
        previous_checkpoint = Some(checkpoint.checkpoint_id);
        let learning = service.learning_status("acceptance").await?;
        ensure!(
            learning.observations.sessions.len() == 1,
            "a second checkpoint must replace the same session contribution"
        );
        let observation = learning
            .observations
            .sessions
            .values()
            .next()
            .context("observation missing")?;
        ensure!(
            observation.quality.is_some(),
            "integrated quality missing: {:?}",
            learning.unavailable
        );
        let cost = observation
            .total_cost_micro_usd
            .context("integrated cost missing")?;
        let execution_cost = executions.iter().try_fold(0_u64, |total, execution| {
            let cost = execution
                .settlement
                .as_ref()
                .and_then(|settled| settled.total_cost_micro_usd)
                .context("actual request cost missing")?;
            total.checked_add(cost).context("fixture cost overflow")
        })?;
        let mut prefix_cost = 0_u64;
        for execution in &executions {
            let included = resource
                .requests
                .iter()
                .any(|request| request.request_id == execution.request_id);
            ensure!(
                included || execution.captured_watermark >= checkpoint.watermark,
                "a request preceding the frozen content boundary was omitted"
            );
            if included {
                let settled = execution
                    .settlement
                    .as_ref()
                    .and_then(|settled| settled.total_cost_micro_usd)
                    .context("prefix request settlement missing")?;
                prefix_cost = prefix_cost
                    .checked_add(settled)
                    .context("prefix cost overflow")?;
            }
        }
        ensure!(
            resource.requests.iter().all(|request| executions
                .iter()
                .any(|execution| execution.request_id == request.request_id)),
            "prefix resources contain an unobserved execution"
        );
        ensure!(
            cost == prefix_cost && cost > previous_cost,
            "continued prefix cost did not accumulate its actual requests exactly once: learning {cost}; prefix {prefix_cost}; all recorded {execution_cost}; previous {previous_cost}"
        );
        previous_cost = cost;
        let judge_costs = verify_judge_costs(fixture, turn).await?;
        eprintln!(
            "{}: turn {turn}; {} actual coding requests; one effective session; checkpoint coding cost {cost} micro-USD; all recorded coding cost {execution_cost} micro-USD; judge cost {:?} micro-USD",
            handle.agent_id,
            inbound.len(),
            judge_costs.total_cost_micro_usd
        );
    }
    ensure!(
        fixture
            .upstream
            .received_requests()
            .await
            .is_some_and(|requests| !requests.is_empty()),
        "no local upstream traffic"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the maintained Codex ACP package and worker; run explicitly with isolated adapter paths"]
async fn maintained_codex_canonical_evolution_acceptance() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
    .await
}

#[tokio::test]
#[ignore = "requires the maintained Claude ACP package and worker; run explicitly with isolated adapter paths"]
async fn maintained_claude_canonical_evolution_acceptance() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
    .await
}
