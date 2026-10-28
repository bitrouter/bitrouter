//! Opt-in CLI acceptance with maintained ACP adapters and real workers.
//! Upstream replies and rubric labels are controlled fixtures, not calibration.

#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use bitrouter::acp_trajectory::CanonicalStore;
use bitrouter::daemon::{AcpControlPlane, NoopObserveStatus, NoopReloader};
use bitrouter::gateway::routing::blocks::allocation::BanditConfig;
use bitrouter::gateway::routing::blocks::control::{BlockStatus, EvolutionMode};
use bitrouter::storage::session_identity::SessionIdentity;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(90);

struct ControlServer(tokio::task::JoinHandle<Result<()>>);
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Environment {
    _directory: tempfile::TempDir,
    config_path: PathBuf,
    home_path: PathBuf,
    temporary_path: PathBuf,
}
impl Environment {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir_in("/tmp")?;
        let config_path = directory.path().join("bitrouter.yaml");
        let home_path = directory.path().join("home");
        let temporary_path = directory.path().join("tmp");
        std::fs::create_dir(&home_path)?;
        std::fs::create_dir(&temporary_path)?;
        Ok(Self {
            _directory: directory,
            config_path,
            home_path,
            temporary_path,
        })
    }
    fn command(&self) -> Result<Command> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
        command
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &self.home_path)
            .env("BITROUTER_HOME", self._directory.path())
            .env("TMPDIR", &self.temporary_path)
            .env("NO_COLOR", "1")
            .current_dir(self._directory.path());
        Ok(command)
    }
}
fn inherited_path() -> Result<OsString> {
    std::env::var_os("PATH").context("fixture PATH missing")
}
fn shell_word(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

struct Cli {
    mock: Environment,
    run_id: String,
}
impl Cli {
    fn launch(mock: Environment, agent: &str, runtime: &tokio::runtime::Runtime) -> Result<Self> {
        let config =
            bitrouter::config::parse_with(&std::fs::read_to_string(&mock.config_path)?, |_| None)?;
        let socket = bitrouter::daemon::socket_path_for(
            &bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
            &config,
        );
        // The fixture owner runs in-process. Strip its ambient environment at
        // the existing launch boundary; only explicit transport env survives.
        // All subsequent control/scoring/publication operations use the CLI.
        let strip_inherited_env = std::env::vars_os()
            .map(|(key, _)| {
                key.into_string()
                    .map_err(|_| anyhow::anyhow!("fixture environment names must be UTF-8"))
            })
            .collect::<Result<Vec<_>>>()?;
        let client = bitrouter::agent_sessions::BackgroundClient::new(socket);
        let started = runtime.block_on(client.start(
            bitrouter::client::sessions::wire::StartRunRequest {
                action_request_id: uuid::Uuid::new_v4().to_string(),
                client_id: None,
                label: None,
                agent_id: agent.into(),
                prompt: None,
                cwd: mock._directory.path().to_path_buf(),
                routing: bitrouter::commands::agent_options::RoutingOptions {
                    model: Some(selector(agent).into()),
                    no_start: true,
                    ..Default::default()
                },
                launch: bitrouter::acp_cli::LaunchOptions {
                    strip_inherited_env,
                    turn_timeout: Some(Duration::from_secs(30)),
                    ..Default::default()
                },
                session: Default::default(),
                presentation: bitrouter::client::sessions::wire::Presentation::Background,
                parent_run_id: None,
                allow_shared_directory: false,
                permission_policy: Default::default(),
                result_schema: None,
            },
        ))?;
        let run_id = started.snapshot.run_id;
        let cli = Self { mock, run_id };
        cli.idle()?;
        Ok(cli)
    }
    fn output(mock: &Environment, args: &[&str]) -> Result<Value> {
        let mut command = mock.command()?;
        if args.starts_with(&["acp", "evolution"]) {
            command
                .args(["acp", "evolution", "--config"])
                .arg(&mock.config_path)
                .args(&args[2..]);
        } else {
            command.args(args).arg("--config").arg(&mock.config_path);
        }
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "CLI {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    fn run(&self, args: &[&str]) -> Result<Value> {
        Self::output(&self.mock, args)
    }
    fn evolution(&self, args: &[&str]) -> Result<Value> {
        let mut all = vec!["acp", "evolution"];
        all.extend_from_slice(args);
        self.run(&all)
    }
    fn idle(&self) -> Result<Value> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let view = self.run(&["agents", "show", &self.run_id])?;
            ensure!(
                view["payload"]["failure"].is_null(),
                "supervised fixture failed: {view}"
            );
            for pending in view["payload"]["pending_permissions"]
                .as_array()
                .context("pending permissions missing")?
            {
                let option = pending["options"]
                    .as_array()
                    .context("offered options missing")?
                    .iter()
                    .find(|option| option["kind"] == "allow_once")
                    .context("fixture has no one-time approval")?;
                self.run(&[
                    "agents",
                    "answer",
                    &self.run_id,
                    "--request",
                    pending["permission_id"]
                        .as_str()
                        .context("permission id missing")?,
                    "--option",
                    option["optionId"].as_str().context("option id missing")?,
                ])?;
            }
            if view["payload"]["turn"] == "idle" && view["payload"]["native_session_id"].is_string()
            {
                return Ok(view);
            }
            ensure!(
                Instant::now() < deadline,
                "supervised fixture did not become idle: {view}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn submit(&self, prompt: &str) -> Result<()> {
        let file = self.mock._directory.path().join("prompt.txt");
        std::fs::write(&file, prompt)?;
        self.run(&[
            "agents",
            "prompt",
            &self.run_id,
            "--prompt-file",
            file.to_str().context("prompt encoding")?,
        ])?;
        self.idle()?;
        Ok(())
    }
    fn new_session(&self) -> Result<()> {
        self.run(&["agents", "session", "new", &self.run_id])?;
        self.idle()?;
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        self.run(&["agents", "stop", &self.run_id])?;
        Ok(())
    }
}
impl Drop for Cli {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

use bitrouter::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
use bitrouter::core::evolution::evidence::{EvidenceKind, EvidencePacket};
use bitrouter::core::evolution::rubric::{self, Applicability, RubricEvaluation, RubricItem};
use bitrouter::core::evolution::runtime::EvolutionRuntime;
use bitrouter::core::evolution::scheduler::EvolutionScheduler;
use bitrouter::gateway::http::AppState;
use bitrouter::gateway::http::build_router;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers};

#[path = "native_evolution_cli/coding.rs"]
mod coding;

struct NativeService {
    evolution: EvolutionRuntime,
    canonical: CanonicalStore,
    upstream: MockServer,
    _tasks: Vec<ControlServer>,
}

fn reply(request: &Request) -> Result<ResponseTemplate> {
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
    mock: &Environment,
    agent: &str,
    adapter_env: &str,
    worker_env: &str,
) -> Result<NativeService> {
    setup_with_reply(mock, agent, adapter_env, worker_env, reply).await
}

async fn setup_with_reply(
    mock: &Environment,
    agent: &str,
    adapter_env: &str,
    worker_env: &str,
    responder: fn(&Request) -> Result<ResponseTemplate>,
) -> Result<NativeService> {
    use bitrouter::config::agent::AcpAgentConfig;
    use bitrouter::config::agent::AcpTransport;
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
    let harness = bitrouter::harness::catalog::by_id(agent).context("unknown harness")?;
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let listen = listener.local_addr()?.to_string();
    let selector = selector(agent);
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
        mock._directory.path().join("canonical.db").display(),
        upstream.uri()
    );
    let mut env = HashMap::from([
        (
            "PATH".into(),
            inherited_path()?
                .into_string()
                .map_err(|_| anyhow::anyhow!("fixture PATH must be UTF-8"))?,
        ),
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
    let config = bitrouter::config::parse_with(&config_text, |_| None)?;
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
                #[cfg(feature = "harness")]
                runtime: assembled.acp_runtime,
                metering: bitrouter::metering::MeteringStore::new(assembled.db),
                inventory: Some(assembled.gateway_requests.inventory()),
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
    tokio::time::timeout(TIMEOUT, async {
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
        "gpt-5.2"
    } else {
        "coding"
    }
}

async fn read_effective(
    service: &NativeService,
    identity: &SessionIdentity,
) -> Result<bitrouter::acp_trajectory::checkpoint::types::EffectiveAssessment> {
    tokio::time::timeout(TIMEOUT, async {
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
    tokio::time::timeout(TIMEOUT, async {
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

fn create_candidate(code: &mut Cli, agent: &str) -> Result<()> {
    code.evolution(&["mode", "automatic", "--judge-model", "judge"])?;
    let spec = json!({"block_id":"native-terminal-trial","source":agent,"rules":[{"selector":selector(agent),"fingerprint":null,"baseline_route":selector(agent),"challenger_route":"candidate"}],"rationale":"Reduce total session cost","independence_rationale":"Related changes are in this block","feedback":{"kind":"judge","model":"judge"}});
    let path = code.mock._directory.path().join("candidate-spec.json");
    std::fs::write(&path, serde_json::to_vec(&spec)?)?;
    let preview = code.evolution(&[
        "candidate",
        "preview",
        "--from",
        path.to_str().context("spec path")?,
    ])?;
    let path = code.mock._directory.path().join("reviewed-preview.json");
    std::fs::write(&path, serde_json::to_vec(&preview["report"]["report"])?)?;
    code.evolution(&[
        "candidate",
        "register",
        "--from",
        path.to_str().context("preview path")?,
    ])?;
    Ok(())
}

fn trial_sessions(
    code: &mut Cli,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
    agent: &str,
) -> Result<(usize, SessionIdentity)> {
    let mut known = runtime
        .block_on(service.canonical.list("local", agent))?
        .into_iter()
        .map(|session| session.native_session_id)
        .collect::<std::collections::BTreeSet<_>>();
    for count in 1..=64 {
        code.new_session()?;
        let identity = runtime.block_on(async {
            tokio::time::timeout(TIMEOUT, async {
                loop {
                    for session in service.canonical.list("local", agent).await? {
                        if !known.contains(&session.native_session_id) {
                            return Ok::<_, anyhow::Error>(SessionIdentity {
                                owner: "local".into(),
                                source: agent.into(),
                                native_session_id: session.native_session_id,
                            });
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .context("new native session was not recorded")?
        })?;
        known.insert(identity.native_session_id.clone());
        code.submit("Reply exactly NATIVE_TERMINAL_OK. Do not use tools, run tests, request review or create a PR.")?;
        let learning = runtime.block_on(async {
            tokio::time::timeout(TIMEOUT, async {
                loop {
                    let report = match service
                        .evolution
                        .service("local")?
                        .learning_status("native-terminal-trial")
                        .await
                    {
                        Err(error)
                            if error.to_string().contains("changed while reading")
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
                    "run": code.run(&["agents","show",&code.run_id])?,
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

fn restore_candidate(code: &mut Cli) -> Result<()> {
    let state = code.evolution(&["status"])?;
    let block = &state["report"]["control"]["blocks"]["native-terminal-trial"];
    ensure!(block["status"] == "exploring", "unexpected trial state");
    code.evolution(&[
        "restore",
        "native-terminal-trial",
        "--expected-experiment",
        block["experiment_id"].as_str().context("experiment")?,
        "--expected-revision",
        block["revision"].as_str().context("revision")?,
        "--reason",
        "End the controlled native terminal trial",
    ])?;
    Ok(())
}

fn assert_restored_dispatch(
    code: &mut Cli,
    runtime: &tokio::runtime::Runtime,
    service: &NativeService,
    identity: &SessionIdentity,
) -> Result<()> {
    let previous = runtime
        .block_on(service.evolution.executions(identity))?
        .into_iter()
        .map(|execution| execution.request_id)
        .collect::<std::collections::BTreeSet<_>>();
    code.submit("Controlled follow-up after the trial. Reply briefly without tools.")?;
    runtime.block_on(async {
        tokio::time::timeout(TIMEOUT, async {
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
    let mock = Environment::new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let service = runtime.block_on(setup(&mock, agent, adapter, worker))?;
    let mut code = Cli::launch(mock, agent, &runtime)?;
    code.evolution(&["mode", "automatic", "--judge-model", "judge"])?;
    assert_eq!(
        runtime
            .block_on(service.evolution.service("local")?.state())?
            .mode,
        EvolutionMode::Automatic
    );
    code.submit("Reply exactly NATIVE_TERMINAL_OK. Do not use tools, run tests, request review or create a PR.")?;
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
    code.evolution(&["mode", "manual"])?;
    let listed = code.evolution(&[
        "checkpoint",
        "--source",
        agent,
        "--session",
        &identity.native_session_id,
        "list",
    ])?;
    let watermark = listed["report"]["data"]["effective"]["current_watermark"]
        .as_i64()
        .context("watermark")?
        .to_string();
    let frozen = code.evolution(&[
        "checkpoint",
        "--source",
        agent,
        "--session",
        &identity.native_session_id,
        "freeze",
        "--expected-watermark",
        &watermark,
    ])?;
    let input: bitrouter::core::evolution::operator::checkpoint::ReviewInput =
        serde_json::from_value(frozen["report"]["data"].clone())?;
    let mut evaluation = input.previous.context("automatic rubric prefill missing")?;
    let citation = input
        .evidence
        .items
        .iter()
        .find(|item| item.kind == EvidenceKind::AgentMessage)
        .context("recorded reply citation missing")?
        .citation
        .clone();
    let delivery = evaluation
        .items
        .iter_mut()
        .find(|item| item.criterion_id == "delivery")
        .context("delivery criterion missing")?;
    delivery.score = CriterionScore::Scored { value_ppm: 500_000 };
    delivery.evidence = vec![citation];
    delivery.explanation =
        "Controlled manual correction of the recorded reply; fixture score only.".into();
    evaluation.summary = "Native terminal manual correction".into();
    let submission = bitrouter::core::evolution::scoring::RubricSubmission {
        submission_id: "cli-manual-correction".into(),
        checkpoint_id: input.checkpoint.checkpoint_id,
        expected_revision: input.expected_revision,
        source: AssessmentSource::Human,
        evaluator_id: bitrouter::core::evolution::scoring::TUI_EVALUATOR_ID.into(),
        evaluator_version: bitrouter::core::evolution::scoring::TUI_EVALUATOR_VERSION.into(),
        evaluation,
    };
    let file = code.mock._directory.path().join("manual-review.json");
    std::fs::write(&file, serde_json::to_vec(&submission)?)?;
    code.evolution(&[
        "checkpoint",
        "--source",
        agent,
        "--session",
        &identity.native_session_id,
        "submit",
        "--from",
        file.to_str().context("review path")?,
    ])?;
    let manual = runtime.block_on(read_effective(&service, &identity))?;
    assert_ne!(manual.current_revision.as_deref(), Some(prior.as_str()));
    let saved = manual.assessment.context("manual assessment missing")?;
    assert_eq!(saved.input.source, AssessmentSource::Human);
    assert_eq!(saved.input.reason, "Native terminal manual correction");
    assert_eq!(
        runtime
            .block_on(service.canonical.assessment_history(&identity))?
            .len(),
        2
    );

    create_candidate(&mut code, agent)?;
    let (trial_sessions, trial_identity) = trial_sessions(&mut code, &runtime, &service, agent)?;
    restore_candidate(&mut code)?;
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
    assert_restored_dispatch(&mut code, &runtime, &service, &trial_identity)?;

    code.evolution(&["mode", "off"])?;
    assert_eq!(
        runtime
            .block_on(service.evolution.service("local")?.state())?
            .mode,
        EvolutionMode::Off
    );
    code.stop()?;
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
        json!({"agent":agent,"native_session":identity.native_session_id,"initial_coding_requests":executions.len(),"upstream_requests":requests.len(),"trial_sessions":trial_sessions,"manual_revisions":1,"initial_automatic_revisions":1,"cli_control":true,"candidate_dispatched":true,"candidate_withdrawn":true,"baseline_dispatched_after_withdrawal_while_automatic":true,"scope":"maintained worker CLI feedback, trial and operator withdrawal; fixed replies and labels, not calibration or adoption campaign"})
    );

    Ok(())
}
#[test]
#[ignore = "requires the maintained Codex ACP adapter and isolated worker executable"]
fn maintained_codex_cli_feedback_trial_withdrawal_and_off() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
}

#[test]
#[ignore = "requires the maintained Claude ACP adapter and isolated worker executable"]
fn maintained_claude_cli_feedback_trial_withdrawal_and_off() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
}
