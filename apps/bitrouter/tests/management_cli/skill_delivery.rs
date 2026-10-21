//! Daemon-supervised skill delivery; independent upstream parsers, temp-only state.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;

use super::fixture::example_binary;
use super::{Fixture, text_path as path};

async fn fixture(profile: &str, options: &[&str]) -> Result<Fixture> {
    static CODEX: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    static CLAUDE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let binary = if profile == "claude-acp" {
        CLAUDE.get_or_init(|| {
            example_binary::build("bitrouter-orchestrator", "acp_claude_skill_fixture")
        })
    } else {
        CODEX.get_or_init(|| {
            example_binary::build("bitrouter-orchestrator", "acp_codex_skill_fixture")
        })
    }
    .as_ref()
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut fixture = Fixture::new(None, None).await?;
    fixture.stop(false).await?;
    let mut config: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    let marker = match profile {
        "claude-acp" => "@agentclientprotocol/claude-agent-acp@0.75.1",
        "codex-acp" => "@agentclientprotocol/codex-acp@1.10.0",
        _ => "unknown-fixture",
    };
    let mut args = vec![
        marker.to_string(),
        "--journal".into(),
        path(&fixture.capture)?.into(),
    ];
    args.extend(options.iter().map(|arg| (*arg).to_string()));
    config["agents"] =
        json!({"stub":{"name":"stub","transport":{"type":"stdio","command":binary,"args":args}}});
    std::fs::write(&fixture.config, serde_json::to_vec_pretty(&config)?)?;
    fixture.start().await?;
    Ok(fixture)
}

async fn install(fixture: &Fixture, marker: &str) -> Result<(PathBuf, String)> {
    let source = fixture.directory.path().join("source/review");
    std::fs::create_dir_all(source.join("references"))?;
    std::fs::write(
        source.join("SKILL.md"),
        format!(
            "---\nname: review\ndescription: review workflow\nallowed-tools: shell\n---\n{marker}\n"
        ),
    )?;
    std::fs::write(source.join("references/check.md"), "original support")?;
    let report = fixture
        .run(&[
            "init",
            "--yes",
            "--install-skill",
            path(&source)?,
            "--workspace",
            path(&fixture.workspace)?,
            "--after",
            "exit",
        ])
        .await?;
    let installation = report["resources"]
        .as_array()
        .context("onboarding resources")?
        .iter()
        .find(|row| row["kind"] == "skill" && row["status"] == "installed")
        .context("onboarding installed skill")?;
    let installed = installation["receipt"]["skills"]
        .as_array()
        .context("installation rows")?
        .iter()
        .find(|row| row["ownership"] == "managed" && row["installed"] == true)
        .context("managed row")?;
    let id = installed["installation_id"]
        .as_str()
        .context("installed identity")?;
    Ok((source, format!("project:{id}")))
}

async fn idle(fixture: &Fixture, run: &str) -> Result<Value> {
    fixture
        .wait(&["agents", "show", run], |value| {
            value["payload"]["turn"] == "idle"
        })
        .await
}

fn delivered(snapshot: &Value, index: usize) -> Result<&Value> {
    let report = &snapshot["payload"]["skill_deliveries"][index];
    ensure!(
        report["stage"] == "delivered",
        "skill delivery not completed: {report}; snapshot={snapshot}"
    );
    Ok(report)
}

#[tokio::test]
async fn codex_cli_delivers_exact_local_binding_and_retains_two_versions_after_restart()
-> Result<()> {
    let mut fixture = fixture("codex-acp", &[]).await?;
    let (source, selector) = install(&fixture, "CODEX_ORIGINAL").await?;
    let run = fixture
        .background("review code", &["--direct", "--skill", &selector])
        .await?;
    let first = idle(&fixture, &run).await?;
    let original = delivered(&first, 0)?.clone();
    assert_eq!(original["capabilities"]["adapter"], "codex_text");
    assert_eq!(original["receipt"]["sources"][0]["kind"], "managed");
    let model = fixture
        .captured()?
        .into_iter()
        .find(|row| row["event"] == "codex_model_input")
        .context("model input")?;
    let frame = model["inputs"][0]["text"].as_str().context("root text")?;
    ensure!(frame.contains("CODEX_ORIGINAL"));
    let version = original["receipt"]["sources"][0]["binding"]["version"]
        .as_str()
        .context("original version")?;
    ensure!(frame.contains(version));
    // Change a support file only: the whole-bundle version must still change.
    std::fs::write(source.join("references/check.md"), "updated support")?;
    fixture
        .run(&[
            "skills",
            "update",
            "review",
            "--workspace",
            path(&fixture.workspace)?,
        ])
        .await?;
    let followup = fixture.prompt("second.txt", "review again")?;
    fixture
        .run(&["agents", "prompt", &run, "--prompt-file", path(&followup)?])
        .await?;
    let second = idle(&fixture, &run).await?;
    let newer = delivered(&second, 1)?;
    ensure!(newer["receipt"]["sources"][0]["binding"]["version"] != version);
    assert_eq!(second["payload"]["skill_deliveries"][0], original);
    assert_eq!(
        fixture
            .captured()?
            .iter()
            .filter(|row| row["event"] == "codex_model_input")
            .count(),
        2
    );
    fixture
        .run(&[
            "skills",
            "uninstall",
            "review",
            "--workspace",
            path(&fixture.workspace)?,
        ])
        .await?;
    let missing = fixture.prompt("missing.txt", "required skill is now unavailable")?;
    fixture
        .run(&["agents", "prompt", &run, "--prompt-file", path(&missing)?])
        .await?;
    let failed = idle(&fixture, &run).await?;
    ensure!(
        failed["payload"]["failure"]
            .as_str()
            .is_some_and(|failure| failure.contains("required skill"))
    );
    assert_eq!(
        fixture
            .captured()?
            .iter()
            .filter(|row| row["event"] == "codex_model_input")
            .count(),
        2
    );
    let original_reports = failed["payload"]["skill_deliveries"].clone();
    fixture.stop(false).await?;
    let requests = fixture.captured()?;
    fixture.start().await?;
    let cold = fixture.run(&["agents", "show", &run]).await?;
    assert_eq!(cold["payload"]["skill_deliveries"], original_reports);
    assert_eq!(fixture.captured()?, requests);
    fixture.stop(false).await
}

#[tokio::test]
async fn claude_cli_uses_negotiated_embedded_data_and_keeps_source_receipt_in_original_ledger()
-> Result<()> {
    let mut fixture = fixture("claude-acp", &[]).await?;
    let (_, selector) = install(&fixture, "CLAUDE_ORIGINAL").await?;
    let run = fixture
        .background("review code", &["--direct", "--skill", &selector])
        .await?;
    let snapshot = idle(&fixture, &run).await?;
    let report = delivered(&snapshot, 0)?;
    assert_eq!(report["capabilities"]["adapter"], "claude_embedded_context");
    assert_eq!(report["capabilities"]["native_activation"], false);
    assert_eq!(report["capabilities"]["verified_bundle_access"], false);
    assert_eq!(report["capabilities"]["mcp_origin"], false);
    let captured = fixture.captured()?;
    let sent = captured
        .iter()
        .find(|row| row["method"] == "session/prompt")
        .context("actual ACP prompt")?;
    assert_eq!(sent["params"]["prompt"][0]["type"], "resource");
    let model = captured
        .iter()
        .find(|row| row["event"] == "claude_user_message")
        .context("actual fixture message")?;
    assert_eq!(model["message"]["role"], "user");
    ensure!(model["message"].to_string().contains("CLAUDE_ORIGINAL"));
    let human = fixture.output(&["agents", "show", &run, "--human"]).await?;
    ensure!(human.status.success() && !human.stdout.contains(&0x1b));
    let human_text = String::from_utf8(human.stdout)?;
    ensure!(human_text.contains("claude_embedded_context"));
    ensure!(!human_text.contains("CLAUDE_ORIGINAL"));
    let paths = fixture.run(&["agents", "list"]).await?;
    assert_eq!(paths["skill_delivery"]["native_launcher"], "unsupported");
    assert_eq!(paths["skill_delivery"]["inference_only"], "unsupported");
    fixture.stop(false).await
}

#[tokio::test]
async fn required_skills_fail_without_a_prompt_for_unknown_or_unadvertised_adapter() -> Result<()> {
    for (profile, options) in [("unknown", vec![]), ("claude-acp", vec!["--no-context"])] {
        let mut fixture = fixture(profile, &options).await?;
        let (_, selector) = install(&fixture, "UNSUPPORTED").await?;
        let run = fixture
            .background("do not dispatch", &["--direct", "--skill", &selector])
            .await?;
        let rejected = idle(&fixture, &run).await?;
        ensure!(
            rejected["payload"]["failure"]
                .as_str()
                .is_some_and(|failure| failure.contains("unsupported")),
            "{rejected}"
        );
        assert!(
            !fixture
                .captured()?
                .iter()
                .any(|row| row["method"] == "session/prompt")
        );
        fixture.stop(false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn mcp_constraint_and_unsupervised_delivery_are_rejected_before_adapter_requests()
-> Result<()> {
    let mut fixture = fixture("codex-acp", &[]).await?;
    let (_, selector) = install(&fixture, "NEVER_SENT").await?;
    let mcp = fixture
        .rejected(&[
            "run",
            "stub",
            "reject MCP",
            "--direct",
            "--background",
            "--approve-all",
            "--skill",
            "mcp:approved:skill://review/SKILL.md",
        ])
        .await?;
    ensure!(mcp.to_string().contains("unsupported"));
    let direct = fixture
        .rejected(&[
            "run",
            "stub",
            "reject direct",
            "--direct",
            "--skill",
            &selector,
        ])
        .await?;
    ensure!(direct.to_string().contains("daemon-owned --background"));
    assert!(fixture.captured()?.is_empty());
    fixture.stop(false).await
}

#[tokio::test]
async fn both_adapters_keep_tool_approval_separate_from_local_skill_allowed_tools() -> Result<()> {
    for profile in ["codex-acp", "claude-acp"] {
        for (policy, allowed) in [("--approve-reads", false), ("--approve-all", true)] {
            let mut fixture = fixture(profile, &["--permission"]).await?;
            let (_, selector) = install(&fixture, "LOCAL_ALLOWED_TOOLS_IS_NOT_A_GRANT").await?;
            let run = fixture
                .background(
                    "review with a command",
                    &["--direct", "--skill", &selector, policy],
                )
                .await?;
            let snapshot = idle(&fixture, &run).await?;
            delivered(&snapshot, 0)?;
            let captured = fixture.captured()?;
            let answer = captured
                .iter()
                .find(|row| {
                    row["method"].is_null()
                        && (row["id"] == "codex-permission" || row["id"] == "claude-permission")
                })
                .context("permission response")?;
            assert_eq!(
                answer["result"]["outcome"]["optionId"],
                if allowed { "allow" } else { "deny" }
            );
            assert_eq!(fixture.capture.with_extension("effect").exists(), allowed);
            fixture.stop(false).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn both_daemon_owned_adapters_cancel_the_original_prepared_delivery_without_replay()
-> Result<()> {
    use std::time::Duration;
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &["--hold"]).await?;
        let (_, selector) = install(&fixture, "CANCEL_ORIGINAL").await?;
        let run = fixture
            .background(
                "review until cancelled",
                &["--direct", "--skill", &selector],
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if fixture.captured()?.iter().any(|row| {
                    row["event"] == "codex_model_input" || row["event"] == "claude_user_message"
                }) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let before = fixture.run(&["agents", "show", &run]).await?;
        let original = before["payload"]["skill_deliveries"][0]["receipt"].clone();
        fixture.run(&["agents", "cancel", &run]).await?;
        let after = idle(&fixture, &run).await?;
        assert_eq!(
            after["payload"]["skill_deliveries"][0]["stage"],
            "cancelled"
        );
        assert_eq!(after["payload"]["skill_deliveries"][0]["receipt"], original);
        assert_eq!(
            fixture
                .captured()?
                .iter()
                .filter(|row| row["method"] == "session/prompt")
                .count(),
            1
        );
        fixture.stop(false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn each_adapter_rejects_required_over_budget_instructions_without_partial_prompt()
-> Result<()> {
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &[]).await?;
        let (source, selector) = install(&fixture, "BOUNDED").await?;
        std::fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: review\ndescription: review workflow\n---\n{}",
                "large ".repeat(14000)
            ),
        )?;
        fixture
            .run(&[
                "skills",
                "update",
                "review",
                "--workspace",
                path(&fixture.workspace)?,
            ])
            .await?;
        let run = fixture
            .background("do not truncate", &["--direct", "--skill", &selector])
            .await?;
        let state = idle(&fixture, &run).await?;
        ensure!(
            state["payload"]["failure"]
                .as_str()
                .is_some_and(|error| error.contains("skill_instruction_budget")),
            "{state}"
        );
        assert!(
            !fixture
                .captured()?
                .iter()
                .any(|row| row["method"] == "session/prompt")
        );
        fixture.stop(false).await?;
    }
    Ok(())
}

async fn configure_decisions(fixture: &mut Fixture, base_url: &str) -> Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    fixture.stop(false).await?;
    let mut config: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    config["decision_model"] = json!({"model":"acp-workflow-fixture","base_url":base_url,
        "api_key_env":"BITROUTER_SKILL_DECISION_FIXTURE_KEY"});
    std::fs::write(&fixture.config, serde_json::to_vec_pretty(&config)?)?;
    fixture.daemon = Some(
        fixture
            .command()
            .env("BITROUTER_SKILL_DECISION_FIXTURE_KEY", "fake-local-only")
            .args(["serve", "--config"])
            .arg(&fixture.config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(child) = fixture.daemon.as_mut()
                && let Some(status) = child.try_wait()?
            {
                anyhow::bail!("decision fixture daemon exited {status}");
            }
            if bitrouter::daemon::probe_status(&fixture.socket)
                .await?
                .is_some()
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    Ok(())
}

fn decision_response(request: &Value) -> Value {
    let relevant = request["state"]["task"]
        .as_str()
        .is_some_and(|task| task.contains("review"));
    let answers: serde_json::Map<_, _> = request["questions"]
        .as_object()
        .into_iter()
        .flat_map(|questions| questions.keys())
        .map(|id| {
            (
                id.clone(),
                json!({"type":"noul","noul":if relevant {0.99}else{0.01}}),
            )
        })
        .collect();
    json!({"model":request["model"],"answers":answers,"usage":{"input_tokens":17,"output_tokens":3}})
}

struct DecisionGate {
    url: String,
    request: tokio::sync::oneshot::Receiver<Value>,
    release: tokio::sync::oneshot::Sender<()>,
    worker: tokio::task::JoinHandle<Result<()>>,
}

impl DecisionGate {
    async fn start() -> Result<Self> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let (received, request) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut stream = BufReader::new(stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                ensure!(stream.read_line(&mut line).await? > 0, "HTTP header EOF");
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>()?);
                }
            }
            let length = length.context("decision body length")?;
            ensure!(length <= 1024 * 1024, "fixture body bound");
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await?;
            let request: Value = serde_json::from_slice(&body)?;
            let response = serde_json::to_vec(&decision_response(&request))?;
            received
                .send(request)
                .map_err(|_| anyhow::anyhow!("gate receiver closed"))?;
            released.await?;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            // A cancelled decision may already have closed its original socket.
            for bytes in [header.as_bytes(), response.as_slice()] {
                if let Err(error) = stream.get_mut().write_all(bytes).await {
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) {
                        return Ok(());
                    }
                    return Err(error.into());
                }
            }
            Ok(())
        });
        Ok(Self {
            url,
            request,
            release,
            worker,
        })
    }
}

#[tokio::test]
async fn both_adapters_block_dispatch_after_cancel_or_unacknowledged_decision_outcome() -> Result<()>
{
    use std::time::Duration;
    for profile in ["codex-acp", "claude-acp"] {
        for fail_journal in [false, true] {
            let mut fixture = fixture(profile, &[]).await?;
            install(&fixture, "MUST_NOT_DISPATCH").await?;
            let gate = DecisionGate::start().await?;
            configure_decisions(&mut fixture, &gate.url).await?;
            let run = fixture
                .background("review gated input", &["--direct"])
                .await?;
            let request = tokio::time::timeout(Duration::from_secs(5), gate.request).await??;
            assert_eq!(request["state"]["task"], "review gated input");
            let intended = fixture.run(&["agents", "show", &run]).await?;
            assert_eq!(
                intended["payload"]["skill_deliveries"][0]["stage"],
                "decision_intended"
            );
            let original = &intended["payload"]["skill_deliveries"][0];
            ensure!(original["decision"]["outcome"].is_null());
            if fail_journal {
                let ledger = std::fs::read_dir(fixture.directory.path())?
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name().is_some_and(|name| {
                            name.to_string_lossy().starts_with("supervisor-runs-v1-")
                                && name.to_string_lossy().ends_with(".json")
                        })
                    })
                    .context("owned supervisor ledger")?;
                let backup = ledger.with_extension("saved");
                std::fs::rename(&ledger, &backup)?;
                std::fs::create_dir(&ledger)?;
                gate.release
                    .send(())
                    .map_err(|_| anyhow::anyhow!("gate closed"))?;
                let failed = idle(&fixture, &run).await?;
                ensure!(
                    failed["payload"]["failure"]
                        .as_str()
                        .is_some_and(|error| error.contains("ledger")),
                    "{failed}"
                );
                ensure!(failed["payload"]["skill_deliveries"][0]["receipt"].is_null());
                let durable: Value = serde_json::from_slice(&std::fs::read(&backup)?)?;
                let row = durable["runs"]
                    .as_array()
                    .context("ledger rows")?
                    .iter()
                    .find(|row| row["run_id"] == run)
                    .context("original durable row")?;
                assert_eq!(row["skill_deliveries"][0], *original);
                std::fs::remove_dir(&ledger)?;
                std::fs::rename(&backup, &ledger)?;
            } else {
                fixture.run(&["agents", "cancel", &run]).await?;
                let cancelled = idle(&fixture, &run).await?;
                let report = &cancelled["payload"]["skill_deliveries"][0];
                assert_eq!(report["stage"], "cancelled");
                assert_eq!(
                    report["decision"]["request"],
                    original["decision"]["request"]
                );
                assert_eq!(report["decision"]["outcome"]["Err"]["may_have_run"], true);
                ensure!(report["receipt"].is_null());
                gate.release
                    .send(())
                    .map_err(|_| anyhow::anyhow!("gate closed"))?;
            }
            tokio::time::timeout(Duration::from_secs(5), gate.worker).await???;
            assert!(
                !fixture
                    .captured()?
                    .iter()
                    .any(|row| row["method"] == "session/prompt")
            );
            fixture.stop(false).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn legacy_supervisor_constraints_are_rejected_before_authority_or_adapter_effects()
-> Result<()> {
    use bitrouter::daemon::{DaemonCommand, DaemonResponse};
    use bitrouter::supervisor::{Presentation, SessionCommand, SessionRequest, StartRunRequest};
    let mut fixture = fixture("codex-acp", &[]).await?;
    for version in [1, 2] {
        let request = SessionRequest {
            version,
            authorization: None,
            command: SessionCommand::Start(Box::new(StartRunRequest {
                action_request_id: "legacy-skill-must-not-start".into(),
                client_id: None,
                label: None,
                agent_id: "stub".into(),
                prompt: Some("review".into()),
                cwd: fixture.workspace.clone(),
                routing: Default::default(),
                launch: bitrouter::acp_cli::LaunchOptions {
                    skills: Some(Default::default()),
                    ..Default::default()
                },
                session: Default::default(),
                presentation: Presentation::Background,
                parent_run_id: None,
                allow_shared_directory: false,
                permission_policy: Default::default(),
                result_schema: None,
            })),
        };
        let response =
            bitrouter::daemon::send_command(&fixture.socket, &DaemonCommand::Sessions { request })
                .await?;
        let DaemonResponse::Error { message } = response else {
            anyhow::bail!("legacy constraints accepted");
        };
        ensure!(message.contains("requires session protocol version 3"));
    }
    ensure!(fixture.captured()?.is_empty());
    fixture.stop(false).await
}

#[tokio::test]
async fn workflow_choice_and_explicit_exclusion_use_the_original_daemon_decision_runtime()
-> Result<()> {
    use wiremock::matchers::{method, path as request_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &[]).await?;
        let (_, selector) = install(&fixture, "AUTO_SELECTED_ORIGINAL").await?;
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(request_path("/v1/systemone"))
            .respond_with(|request: &wiremock::Request| {
                match serde_json::from_slice::<Value>(&request.body) {
                    Ok(request) => {
                        ResponseTemplate::new(200).set_body_json(decision_response(&request))
                    }
                    Err(_) => ResponseTemplate::new(400),
                }
            })
            .mount(&api)
            .await;
        configure_decisions(&mut fixture, &api.uri()).await?;
        let run = fixture.background("review the task", &["--direct"]).await?;
        let first = idle(&fixture, &run).await?;
        let report = delivered(&first, 0)?;
        assert_eq!(report["candidates"][0]["reason"], "workflow_match");
        assert_eq!(
            report["decision"]["outcome"]["Ok"]["usage"]["input_tokens"],
            17
        );
        assert_eq!(
            api.received_requests()
                .await
                .context("decision requests")?
                .len(),
            1
        );
        let other = fixture.prompt("unrelated.txt", "describe a sunset")?;
        fixture
            .run(&["agents", "prompt", &run, "--prompt-file", path(&other)?])
            .await?;
        let second = idle(&fixture, &run).await?;
        assert_eq!(
            second["payload"]["skill_deliveries"][1]["stage"],
            "no_selection"
        );
        assert_eq!(
            second["payload"]["skill_deliveries"][1]["candidates"][0]["reason"],
            "workflow_no_match"
        );
        assert!(second["payload"]["skill_deliveries"][1]["receipt"].is_null());
        fixture.run(&["agents", "stop", &run]).await?;
        let excluded = fixture
            .background(
                "review but exclude",
                &["--direct", "--exclude-skill", &selector],
            )
            .await?;
        let excluded = idle(&fixture, &excluded).await?;
        assert_eq!(
            excluded["payload"]["skill_deliveries"][0]["candidates"][0]["reason"],
            "explicitly_excluded"
        );
        assert_eq!(
            api.received_requests()
                .await
                .context("decision requests")?
                .len(),
            2
        );
        let prompts: Vec<_> = fixture
            .captured()?
            .into_iter()
            .filter(|row| row["method"] == "session/prompt")
            .collect();
        assert_eq!(prompts.len(), 3);
        let native = fixture.prompt("native-command.txt", "/compact")?;
        let excluded_id = excluded["payload"]["run_id"]
            .as_str()
            .context("excluded run ID")?;
        fixture
            .run(&[
                "agents",
                "prompt",
                excluded_id,
                "--prompt-file",
                path(&native)?,
            ])
            .await?;
        idle(&fixture, excluded_id).await?;
        assert_eq!(
            api.received_requests()
                .await
                .context("decision requests")?
                .len(),
            2
        );
        let after_native = fixture.captured()?;
        let native_wire = after_native
            .iter()
            .rev()
            .find(|row| row["method"] == "session/prompt")
            .context("native command wire")?;
        assert_eq!(
            native_wire["params"]["prompt"],
            json!([{"type":"text","text":"/compact"}])
        );
        fixture.run(&["agents", "stop", excluded_id]).await?;
        let required_command = fixture
            .background("/compact", &["--direct", "--skill", &selector])
            .await?;
        let unsupported = idle(&fixture, &required_command).await?;
        ensure!(
            unsupported["payload"]["failure"]
                .as_str()
                .is_some_and(|failure| failure.contains("native slash-command"))
        );
        assert_eq!(
            fixture
                .captured()?
                .iter()
                .filter(|row| row["method"] == "session/prompt")
                .count(),
            4
        );

        assert!(prompts[0].to_string().contains("AUTO_SELECTED_ORIGINAL"));
        assert!(!prompts[1].to_string().contains("AUTO_SELECTED_ORIGINAL"));
        assert!(!prompts[2].to_string().contains("AUTO_SELECTED_ORIGINAL"));
        fixture.stop(false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn both_adapters_retain_uncertain_original_delivery_after_error_and_cold_restart()
-> Result<()> {
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &["--reject"]).await?;
        let (_, selector) = install(&fixture, "UNCERTAIN_ORIGINAL").await?;
        let run = fixture
            .background("review once", &["--direct", "--skill", &selector])
            .await?;
        let failed = idle(&fixture, &run).await?;
        let report = &failed["payload"]["skill_deliveries"][0];
        assert_eq!(report["stage"], "uncertain");
        ensure!(report["receipt"].is_object());
        ensure!(
            failed["payload"]["failure"]
                .as_str()
                .is_some_and(|error| error.contains("result unavailable after input"))
        );
        let captured = fixture.captured()?;
        assert_eq!(
            captured
                .iter()
                .filter(|row| row["method"] == "session/prompt")
                .count(),
            1
        );
        fixture.stop(false).await?;
        fixture.start().await?;
        let cold = fixture.run(&["agents", "show", &run]).await?;
        assert_eq!(cold["payload"]["skill_deliveries"][0], *report);
        assert_eq!(fixture.captured()?, captured);
        fixture.stop(false).await?;
    }
    Ok(())
}
