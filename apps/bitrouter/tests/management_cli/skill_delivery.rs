//! Daemon-supervised skill delivery; independent upstream parsers, temp-only state.
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;
use wiremock::matchers::{method, path as http_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fixture::example_binary;
use super::{Fixture, text_path as path};

async fn fixture(profile: &str, options: &[&str]) -> Result<Fixture> {
    static CODEX: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    static CLAUDE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let binary = if profile == "claude-acp" {
        CLAUDE.get_or_init(|| example_binary::build("bitrouter", "acp_claude_skill_fixture"))
    } else {
        CODEX.get_or_init(|| example_binary::build("bitrouter", "acp_codex_skill_fixture"))
    }
    .as_ref()
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut fixture = Fixture::prepare(None, None)?;
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

async fn local_skill(fixture: &Fixture, marker: &str) -> Result<(PathBuf, String)> {
    let source = fixture.workspace.join(".agents/skills/review");
    std::fs::create_dir_all(source.join("references"))?;
    std::fs::write(
        source.join("SKILL.md"),
        format!(
            "---\nname: review\ndescription: review workflow\nallowed-tools: shell\n---\n{marker}\n"
        ),
    )?;
    std::fs::write(source.join("references/check.md"), "original support")?;
    let report = fixture
        .run(&["skills", "list", "--workspace", path(&fixture.workspace)?])
        .await?;
    let id = report["skills"][0]["directory_id"]
        .as_str()
        .context("directory identity")?;
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
async fn codex_cli_delivers_directory_fingerprints_and_retains_original_evidence_after_restart()
-> Result<()> {
    let mut fixture = fixture("codex-acp", &[]).await?;
    let (source, selector) = local_skill(&fixture, "CODEX_ORIGINAL").await?;
    let run = fixture
        .background("review code", &["--direct", "--skill", &selector])
        .await?;
    let first = idle(&fixture, &run).await?;
    let original = delivered(&first, 0)?.clone();
    assert_eq!(original["capabilities"]["adapter"], "codex_text");
    assert_eq!(original["receipt"]["sources"][0]["kind"], "local");
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
    std::fs::remove_dir_all(&source)?;
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
    let (_, selector) = local_skill(&fixture, "CLAUDE_ORIGINAL").await?;
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
        let (_, selector) = local_skill(&fixture, "UNSUPPORTED").await?;
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
    let (_, selector) = local_skill(&fixture, "NEVER_SENT").await?;
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
            let (_, selector) = local_skill(&fixture, "LOCAL_ALLOWED_TOOLS_IS_NOT_A_GRANT").await?;
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
        let (_, selector) = local_skill(&fixture, "CANCEL_ORIGINAL").await?;
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
        let (source, selector) = local_skill(&fixture, "BOUNDED").await?;
        std::fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: review\ndescription: review workflow\n---\n{}",
                "large ".repeat(14000)
            ),
        )?;
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

#[tokio::test]
async fn legacy_supervisor_constraints_are_rejected_before_authority_or_adapter_effects()
-> Result<()> {
    use bitrouter::client::sessions::wire::{
        Presentation, SessionCommand, SessionRequest, StartRunRequest,
    };
    use bitrouter::daemon::{DaemonCommand, DaemonResponse};
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
async fn both_adapters_retain_uncertain_original_delivery_after_error_and_cold_restart()
-> Result<()> {
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &["--reject"]).await?;
        let (_, selector) = local_skill(&fixture, "UNCERTAIN_ORIGINAL").await?;
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
#[tokio::test]
async fn configured_decision_backend_is_not_used_for_implicit_acp_skill_selection() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(http_path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    for profile in ["codex-acp", "claude-acp"] {
        let mut fixture = fixture(profile, &[]).await?;
        local_skill(&fixture, "NEVER_AUTO_LOAD").await?;
        configure_decisions(&mut fixture, &upstream.uri()).await?;
        let run = fixture.background("review code", &["--direct"]).await?;
        let snapshot = idle(&fixture, &run).await?;
        ensure!(
            snapshot["payload"]
                .get("skill_deliveries")
                .is_none_or(|rows| rows.as_array().is_some_and(Vec::is_empty))
        );
        let model = fixture
            .captured()?
            .into_iter()
            .filter(|row| {
                row["event"] == "codex_model_input" || row["event"] == "claude_user_message"
            })
            .collect::<Vec<_>>();
        ensure!(!model.is_empty() && !serde_json::to_string(&model)?.contains("NEVER_AUTO_LOAD"));
        fixture.stop(false).await?;
    }
    Ok(())
}
