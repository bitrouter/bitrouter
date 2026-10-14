//! Opt-in session/operator acceptance with maintained ACP adapters and real workers.
//! Upstream replies and rubric labels are controlled fixtures, not calibration.

use super::*;
use bitrouter::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
use bitrouter::evolution::evidence::{EvidenceKind, EvidencePacket};
use bitrouter::evolution::rubric::{self, Applicability, RubricEvaluation, RubricItem};
use bitrouter::evolution::runtime::EvolutionRuntime;
use bitrouter::evolution::scheduler::EvolutionScheduler;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers};

#[path = "native/coding.rs"]
mod coding;

#[path = "native/session.rs"]
mod session;
use session::{NativeFixture, cli, create_candidate, inspect, mode};

#[path = "native/responses.rs"]
mod responses;

struct NativeService {
    evolution: EvolutionRuntime,
    canonical: CanonicalStore,
    upstream: MockServer,
    _tasks: Vec<ControlServer>,
}

fn reply(request: &Request) -> Result<ResponseTemplate> {
    if request.url.path() == "/responses" {
        return responses::reply(request, false);
    }
    let body: Value = serde_json::from_slice(&request.body)?;
    let model = body["model"].as_str().context("model missing")?;
    let content = if model == "judge" {
        let input: Value = serde_json::from_str(
            body["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["content"].as_str())
                .context("judge evidence missing")?,
        )?;
        let packet: EvidencePacket = serde_json::from_value(input["evidence"].clone())?;
        let citations = packet
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.kind,
                    EvidenceKind::UserMessage | EvidenceKind::AgentMessage
                )
            })
            .map(|item| item.citation.clone())
            .collect::<Vec<_>>();
        ensure!(!citations.is_empty(), "no captured native messages");
        serde_json::to_string(&RubricEvaluation {
            rubric_version: rubric::RUBRIC_VERSION.into(),
            items: rubric::library()
                .into_iter()
                .map(|template| RubricItem {
                    criterion_id: template.id.into(),
                    applicability: if template.mandatory {
                        Applicability::Applicable
                    } else {
                        Applicability::NotApplicable
                    },
                    selection_reason:
                        "Controlled literal reply; no tools, tests, review or PR requested.".into(),
                    score: if template.mandatory {
                        CriterionScore::Scored { value_ppm: 950_000 }
                    } else {
                        CriterionScore::NotApplicable
                    },
                    evidence: citations.clone(),
                    explanation: "Fixed terminal acceptance label; not judge calibration.".into(),
                })
                .collect(),
            diagnostics: vec![],
            severe_violation: false,
            violation_evidence: vec![],
            summary: "Controlled native terminal reply.".into(),
        })?
    } else if body
        .to_string()
        .contains("Controlled follow-up after the trial.")
    {
        "NATIVE_BASELINE_OK".into()
    } else {
        "NATIVE_TERMINAL_OK".into()
    };
    let usage = json!({"prompt_tokens":20,"completion_tokens":10,"total_tokens":30});
    if body["stream"].as_bool() == Some(true) {
        let chunks = [
            json!({"id":"terminal-chat","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":content},"finish_reason":null}]}),
            json!({"id":"terminal-chat","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":usage}),
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
        Ok(ResponseTemplate::new(200).set_body_json(json!({"id":"terminal-chat","object":"chat.completion","created":0,"model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],"usage":usage})))
    }
}

async fn setup(
    mock: &MockAcp,
    agent: &str,
    adapter_env: &str,
    worker_env: &str,
) -> Result<NativeService> {
    setup_with_reply(mock, agent, adapter_env, worker_env, reply).await
}

async fn setup_with_reply(
    mock: &MockAcp,
    agent: &str,
    adapter_env: &str,
    worker_env: &str,
    responder: fn(&Request) -> Result<ResponseTemplate>,
) -> Result<NativeService> {
    use bitrouter_sdk::config::agent::{AcpAgentConfig, AcpTransport};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    let adapter = PathBuf::from(
        std::env::var(adapter_env)
            .with_context(|| format!("set {adapter_env} to the pinned adapter entry point"))?,
    );
    let package: Value = serde_json::from_slice(&std::fs::read(
        adapter
            .parent()
            .and_then(|path| path.parent())
            .context("adapter package directory missing")?
            .join("package.json"),
    )?)?;
    let harness = bitrouter::harness::by_id(agent).context("unknown harness")?;
    let (name, version) = harness
        .maintained_adapter_identity()
        .context("adapter is not pinned")?;
    ensure!(
        package["name"] == name && package["version"] == version,
        "adapter does not match maintained pin"
    );
    let worker = std::env::var(worker_env)
        .with_context(|| format!("set {worker_env} to the worker executable"))?;
    let wrapper = mock._directory.path().join("native-adapter");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nexec \"$BITROUTER_TEST_NODE\" \"$BITROUTER_TEST_ADAPTER\"\n",
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    let codex_home = mock.home_path.join("codex");
    std::fs::create_dir(&codex_home)?;
    let upstream = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/chat/completions"))
        .respond_with(move |request: &Request| {
            responder(request).unwrap_or_else(|error| {
                ResponseTemplate::new(500).set_body_string(format!("fixture response: {error:#}"))
            })
        })
        .mount(&upstream)
        .await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/responses"))
        .respond_with(move |request: &Request| {
            responder(request).unwrap_or_else(|error| {
                eprintln!("native Responses fixture rejected the actual request: {error:#}");
                ResponseTemplate::new(500).set_body_string(format!("fixture Responses: {error:#}"))
            })
        })
        .mount(&upstream)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let listen = listener.local_addr()?.to_string();
    let selector = selector(agent);
    let worker_protocol = if agent == "codex-acp" {
        "responses"
    } else {
        "chat_completions"
    };
    let mut config_text = format!(
        r#"
server:
  listen: "{listen}"
  skip_auth: true
  control_socket: native-evolution.sock
database:
  url: "sqlite://{}?mode=rwc"
registry:
  enabled: false
acp_recording:
  enabled: true
providers:
  fixture:
    api_base: "{}"
    api_key: fixture-only
    api_protocol:
      - "judge": chat_completions
      - "*": {worker_protocol}
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
        mock._directory.path().join("canonical.db").display(),
        upstream.uri()
    );
    let mut env = HashMap::from([
        ("HOME".into(), mock.home_path.display().to_string()),
        ("CODEX_HOME".into(), codex_home.display().to_string()),
        ("TMPDIR".into(), mock.temporary_path.display().to_string()),
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
    let agent_config = AcpAgentConfig {
        name: agent.into(),
        transport: AcpTransport::Stdio {
            command: wrapper.display().to_string(),
            args: vec![format!("{name}@{version}")],
            env,
        },
    };
    config_text.push_str(&format!(
        "\nagents: {}\n",
        serde_json::to_string(&HashMap::from([(agent, agent_config)]))?
    ));
    std::fs::write(&mock.config_path, &config_text)?;
    let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
    let assembled =
        bitrouter::assemble::build_app_with_path(&config, Some(&mock.config_path)).await?;
    let evolution = assembled.evolution.clone();
    let canonical = CanonicalStore::new(assembled.db.clone());
    let app = Arc::new(assembled.app);
    let pipeline = app.language_model().cloned().context("pipeline missing")?;
    let router = build_router(AppState {
        language_model: pipeline.clone(),
        mcp: app.mcp().cloned(),
        skip_auth: app.skip_auth(),
        metrics_renderer: app.metrics_renderer().cloned(),
        prompt_transforms: app.prompt_transforms().to_vec(),
    });
    let gateway = ControlServer(tokio::spawn(async move {
        axum::serve(listener, router).await?;
        Ok(())
    }));
    let socket = bitrouter::daemon::socket_path_for(
        &bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
        &config,
    );
    let administration = bitrouter::actions::administration::Administration {
        source: bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
        routing: assembled.routing_table.clone(),
        policy: assembled.policy_runtime.clone(),
        observe: assembled.observe.clone(),
    };
    let control = ControlServer(tokio::spawn(
        bitrouter::daemon::run_control_socket_with_acp_runtime_and_administration(
            socket.clone(),
            app,
            listen,
            Arc::new(NoopReloader),
            Arc::new(NoopObserveStatus { compiled_in: false }),
            AcpControlPlane {
                runtime: assembled.acp_runtime,
                metering: bitrouter::metering::MeteringStore::new(assembled.db),
                inventory: Some(evolution.inventory()),
                evolution: Some(evolution.clone()),
            },
            Some(administration),
        ),
    ));
    let scheduler = EvolutionScheduler::new(evolution.clone());
    let worker = ControlServer(tokio::spawn(async move {
        scheduler
            .run(pipeline, tokio_util::sync::CancellationToken::new())
            .await;
        Ok(())
    }));
    tokio::time::timeout(PTY_TIMEOUT, async {
        while bitrouter::daemon::probe_status(&socket).await?.is_none() {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(NativeService {
        evolution,
        canonical,
        upstream,
        _tasks: vec![gateway, control, worker],
    })
}

fn selector(agent: &str) -> &'static str {
    if agent == "codex-acp" {
        "gpt-6.1-sol"
    } else {
        "coding"
    }
}

async fn read_effective(
    service: &NativeService,
    identity: &SessionIdentity,
) -> Result<bitrouter::acp_trajectory::checkpoint::types::EffectiveAssessment> {
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            match service.canonical.effective_assessment(identity).await {
                Err(error) if error.to_string().contains("changed while reading") => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                result => return result,
            }
        }
    })
    .await
    .context("native effective assessment remained unstable")?
}

async fn evaluated(service: &NativeService, agent: &str) -> Result<SessionIdentity> {
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            let sessions = service.canonical.list("local", agent).await?;
            if sessions.len() == 1 {
                let identity = SessionIdentity {
                    owner: "local".into(),
                    source: agent.into(),
                    native_session_id: sessions[0].native_session_id.clone(),
                };
                let effective = read_effective(service, &identity).await?;
                if !effective.stale
                    && effective.assessment.is_some()
                    && effective
                        .resource
                        .as_ref()
                        .is_some_and(|r| r.metering_complete && !r.requests.is_empty())
                {
                    return Ok(identity);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("native terminal feedback did not settle")?
}

fn trial_sessions(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
    agent: &str,
) -> Result<(usize, SessionIdentity)> {
    for count in 1..=64 {
        let identity = runtime.block_on(code.fresh(agent))?;
        runtime.block_on(code.prompt("Reply exactly NATIVE_TERMINAL_OK. Do not use tools, run tests, request review or create a PR.", "NATIVE_TERMINAL_OK", false))?;
        let learning = runtime.block_on(async {
            tokio::time::timeout(PTY_TIMEOUT, async {
                loop {
                    let report = match service
                        .evolution
                        .service("local")?
                        .learning_status("native-terminal-trial")
                        .await
                    {
                        Err(error)
                            if error.to_string().contains("changed while reading")
                                || error.to_string().contains(
                                    "resource observations use different gateway revisions",
                                )
                                || error.to_string().ends_with("; retry") =>
                        {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            continue;
                        }
                        result => result?,
                    };
                    if report
                        .observations
                        .sessions
                        .get(&identity.key()?)
                        .is_some_and(|observation| {
                            observation.quality.is_some()
                                && observation.total_cost_micro_usd.is_some()
                                && observation.latency_ms.is_some()
                        })
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .context("native trial did not enter comparable learning")?
        });
        if let Err(error) = learning {
            let report = runtime.block_on(
                service
                    .evolution
                    .service("local")?
                    .learning_status("native-terminal-trial"),
            )?;
            let key = identity.key()?;
            eprintln!(
                "Native trial {count}: {}",
                json!({
                    "identity": identity,
                    "unavailable": report.unavailable.get(&key),
                    "observation": report.observations.sessions.get(&key),
                    "effective": runtime.block_on(read_effective(service, &identity))?,
                    "run": runtime.block_on(code.api.snapshot())?,
                    "enrollment": runtime.block_on(service.evolution.service("local")?.enrollment(&identity))?,
                    "block": report.block_status,
                    "unavailable_sessions": report.unavailable,
                    "known_observations": report.observations.sessions.keys().collect::<Vec<_>>(),
                })
            );
            return Err(error);
        }
        let executions = runtime.block_on(service.evolution.executions(&identity))?;
        ensure!(
            !executions.is_empty(),
            "trial has no actual gateway request"
        );
        let cheap = executions.iter().any(|execution| {
            execution
                .settlement
                .as_ref()
                .is_some_and(|settlement| settlement.final_model == "cheap")
        });
        if cheap {
            return Ok((count, identity));
        }
    }
    bail!("no candidate dispatch within 64 fresh sessions under the unmodified allocation policy")
}

fn restore_candidate(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
) -> Result<()> {
    inspect(code, runtime, "exploring")?;
    let state = runtime.block_on(service.evolution.service("local")?.state())?;
    let block = state
        .blocks
        .get("native-terminal-trial")
        .context("candidate missing")?;
    cli(
        &code.mock,
        runtime,
        &[
            "acp",
            "evolution",
            "restore",
            "native-terminal-trial",
            "--expected-experiment",
            &block.experiment_id,
            "--expected-revision",
            &block.revision,
            "--reason",
            "End the controlled native terminal trial",
        ],
    )?;
    Ok(())
}

fn assert_restored_dispatch(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
    identity: &SessionIdentity,
) -> Result<()> {
    let previous = runtime
        .block_on(service.evolution.executions(identity))?
        .into_iter()
        .map(|execution| execution.request_id)
        .collect::<std::collections::BTreeSet<_>>();
    runtime.block_on(code.prompt(
        "Controlled follow-up after the trial. Reply briefly without tools.",
        "NATIVE_BASELINE_OK",
        false,
    ))?;
    runtime.block_on(async {
        tokio::time::timeout(PTY_TIMEOUT, async {
            loop {
                let executions = service.evolution.executions(identity).await?;
                let new = executions
                    .iter()
                    .filter(|execution| !previous.contains(&execution.request_id))
                    .collect::<Vec<_>>();
                if !new.is_empty() && new.iter().all(|execution| execution.settlement.is_some()) {
                    ensure!(
                        new.iter().all(|execution| execution
                            .settlement
                            .as_ref()
                            .is_some_and(|settlement| settlement.final_model == "strong"
                                && settlement.final_provider == "fixture")),
                        "withdrawn candidate still served the next native turn"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("restored dispatch did not settle")?
    })
}

fn run(agent: &str, adapter: &str, worker: &str) -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let service = runtime.block_on(setup(&mock, agent, adapter, worker))?;
    let code = runtime.block_on(NativeFixture::start(mock, agent))?;
    mode(&code, &runtime, "automatic")?;
    assert_eq!(
        runtime
            .block_on(service.evolution.service("local")?.state())?
            .mode,
        EvolutionMode::Automatic
    );
    runtime.block_on(code.prompt("Reply exactly NATIVE_TERMINAL_OK. Do not use tools, run tests, request review or create a PR.", "NATIVE_TERMINAL_OK", false))?;
    let identity = runtime.block_on(evaluated(&service, agent))?;
    let automatic = runtime.block_on(read_effective(&service, &identity))?;
    let prior = automatic
        .current_revision
        .context("automatic revision missing")?;
    assert_eq!(
        automatic
            .assessment
            .context("assessment missing")?
            .input
            .source,
        AssessmentSource::Agentic
    );
    mode(&code, &runtime, "manual")?;
    let checkpoint = automatic
        .checkpoint
        .context("automatic checkpoint missing")?
        .checkpoint_id;
    let prepared = cli(
        &code.mock,
        &runtime,
        &[
            "acp",
            "checkpoints",
            "--agent",
            agent,
            &identity.native_session_id,
            "rubric",
            "prepare",
            &checkpoint,
        ],
    )?;
    let packet: EvidencePacket = serde_json::from_value(prepared["data"]["evidence"].clone())?;
    let citation = packet
        .items
        .iter()
        .find(|item| item.kind == EvidenceKind::AgentMessage)
        .context("recorded reply missing")?
        .citation
        .clone();
    let submission = bitrouter::evolution::scoring::RubricSubmission {
        submission_id: "native-cli-manual-correction".into(),
        checkpoint_id: checkpoint,
        expected_revision: Some(prior.clone()),
        source: AssessmentSource::Human,
        evaluator_id: "local-cli-reviewer".into(),
        evaluator_version: "manual-cli-v1".into(),
        evaluation: RubricEvaluation {
            rubric_version: rubric::RUBRIC_VERSION.into(),
            items: rubric::library()
                .into_iter()
                .map(|template| RubricItem {
                    criterion_id: template.id.into(),
                    applicability: if template.mandatory {
                        Applicability::Applicable
                    } else {
                        Applicability::NotApplicable
                    },
                    selection_reason:
                        "Controlled manual correction of the recorded reply; fixture score only."
                            .into(),
                    score: if template.id == "delivery" {
                        CriterionScore::Scored { value_ppm: 500_000 }
                    } else if template.mandatory {
                        CriterionScore::Unknown
                    } else {
                        CriterionScore::NotApplicable
                    },
                    evidence: if template.id == "delivery" {
                        vec![citation.clone()]
                    } else {
                        Vec::new()
                    },
                    explanation:
                        "Controlled manual correction of the recorded reply; fixture score only."
                            .into(),
                })
                .collect(),
            diagnostics: Vec::new(),
            severe_violation: false,
            violation_evidence: Vec::new(),
            summary: "Native CLI manual correction".into(),
        },
    };
    let submission_path = code.mock._directory.path().join("manual-rubric.json");
    std::fs::write(&submission_path, serde_json::to_vec(&submission)?)?;
    cli(
        &code.mock,
        &runtime,
        &[
            "acp",
            "checkpoints",
            "--agent",
            agent,
            &identity.native_session_id,
            "rubric",
            "submit",
            submission_path
                .to_str()
                .context("rubric path is not UTF-8")?,
        ],
    )?;
    let manual = runtime.block_on(read_effective(&service, &identity))?;
    assert_ne!(manual.current_revision.as_deref(), Some(prior.as_str()));
    let saved = manual.assessment.context("manual assessment missing")?;
    assert_eq!(saved.input.source, AssessmentSource::Human);
    assert_eq!(saved.input.reason, "Native CLI manual correction");
    assert_eq!(
        runtime
            .block_on(service.canonical.assessment_history(&identity))?
            .len(),
        2
    );
    create_candidate(&code, &runtime, agent)?;
    let (trial_sessions, trial_identity) = trial_sessions(&code, &runtime, &service, agent)?;
    restore_candidate(&code, &runtime, &service)?;
    let state = runtime.block_on(service.evolution.service("local")?.state())?;
    let block = state
        .blocks
        .get("native-terminal-trial")
        .context("CLI candidate missing")?;
    assert_eq!(block.status, BlockStatus::RolledBack);
    assert_eq!(state.mode, EvolutionMode::Automatic);
    assert_eq!(
        serde_json::to_value(&block.definition.bandit)?,
        serde_json::to_value(BanditConfig::default())?
    );
    let publication = state
        .publications
        .last()
        .context("withdrawal receipt missing")?;
    assert_eq!(publication.action, "operator_restore");
    assert_eq!(
        publication.operator_reason.as_deref(),
        Some("End the controlled native terminal trial")
    );
    assert_restored_dispatch(&code, &runtime, &service, &trial_identity)?;
    mode(&code, &runtime, "off")?;
    assert_eq!(
        runtime
            .block_on(service.evolution.service("local")?.state())?
            .mode,
        EvolutionMode::Off
    );
    runtime.block_on(code.stop())?;
    let executions = runtime.block_on(service.evolution.executions(&identity))?;
    ensure!(
        !executions.is_empty(),
        "native worker made no recorded request"
    );
    let requests = runtime
        .block_on(service.upstream.received_requests())
        .context("upstream requests missing")?;
    let models = requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        models.iter().any(|body| body["model"] == "judge")
            && models.iter().any(|body| body["model"] == "strong"),
        "missing actual coding or judge dispatch"
    );
    println!(
        "{}",
        json!({"agent":agent,"native_session":identity.native_session_id,
        "initial_coding_requests":executions.len(),"upstream_requests":requests.len(),"trial_sessions":trial_sessions,
        "manual_revisions":1,"initial_automatic_revisions":1,"process_stopped":true,
        "candidate_dispatched":true,"candidate_withdrawn":true,"baseline_dispatched_after_withdrawal_while_automatic":true,
        "scope":"maintained worker session and operator CLI; fixed replies and labels, not calibration or adoption campaign"})
    );
    Ok(())
}

#[test]
#[ignore = "requires the maintained Codex ACP adapter and isolated worker executable"]
fn maintained_codex_session_feedback_trial_withdrawal_and_off() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
}

#[test]
#[ignore = "requires the maintained Claude ACP adapter and isolated worker executable"]
fn maintained_claude_session_feedback_trial_withdrawal_and_off() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
}
