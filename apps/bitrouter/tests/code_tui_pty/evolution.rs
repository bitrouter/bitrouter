//! Actual terminal acceptance against an isolated, assembled local control
//! service. The ACP agent is a fixture; model quality is not evaluated here.

use super::*;
use bitrouter::acp_trajectory::{CanonicalStore, SessionIdentity};
use bitrouter::daemon::{AcpControlPlane, NoopObserveStatus, NoopReloader};
use bitrouter::evolution::bandit::BanditConfig;
use bitrouter::evolution::control::{BlockDefinition, BlockRule, BlockStatus, EvolutionMode};

#[path = "evolution/native.rs"]
mod native;

struct ControlServer(tokio::task::JoinHandle<Result<()>>);

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve_control(
    mock: &MockAcp,
    config: &bitrouter_sdk::config::Config,
) -> Result<(
    bitrouter::evolution::runtime::EvolutionRuntime,
    CanonicalStore,
    ControlServer,
)> {
    let assembled =
        bitrouter::assemble::build_app_with_path(config, Some(&mock.config_path)).await?;
    let evolution = assembled.evolution.clone();
    let canonical = CanonicalStore::new(assembled.db.clone());
    let socket = bitrouter::daemon::socket_path_for(
        &bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
        config,
    );
    let server = ControlServer(tokio::spawn(
        bitrouter::daemon::run_control_socket_with_acp_runtime(
            socket.clone(),
            Arc::new(assembled.app),
            "127.0.0.1:1".into(),
            Arc::new(NoopReloader),
            Arc::new(NoopObserveStatus { compiled_in: false }),
            AcpControlPlane {
                runtime: assembled.acp_runtime,
                metering: bitrouter::metering::MeteringStore::new(assembled.db),
                inventory: Some(evolution.inventory()),
                evolution: Some(evolution.clone()),
            },
        ),
    ));
    tokio::time::timeout(PTY_TIMEOUT, async {
        loop {
            if bitrouter::daemon::probe_status(&socket).await?.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok((evolution, canonical, server))
}

#[test]
fn code_fresh_session_preserves_direct_routing_and_timeout_in_the_terminal() -> Result<()> {
    let mock = MockAcp::new(MockScenario::FreshSessions)?;
    let child_pid_path = mock._directory.path().join("code-child.pid");
    let mut command = shell_command(&mock, None)?;
    for arg in [
        "code",
        "stub",
        "--direct",
        "--turn-timeout",
        "1",
        "--config",
    ] {
        command.arg(arg);
    }
    command.arg(&mock.config_path);
    let mut code = CodeFixture {
        mock,
        child_pid_path,
        pty: PtyRunner::spawn(command, CODE_COLUMNS, CODE_ROWS)?,
    };
    code.wait_for_agent_ready()?;
    code.pty.send(b"First native session\r")?;
    code.pty.wait_for_text("FXRP1")?;
    code.pty.wait_for_text("Turn completed")?;
    let first = code.mock.wait_for_request("session/prompt")?;
    code.pty.send(b"\x10")?;
    code.pty.wait_for_text("Commands")?;
    let opening = code.pty.checkpoint();
    code.pty.send(b"New session\r")?;
    let first_id = first["params"]["sessionId"]
        .as_str()
        .context("native id missing")?;
    code.pty.wait_for_screen_inner(
        Some(&opening),
        "a different ready native session",
        |screen| {
            screen.contains("FXRD pty-fresh-")
                && !screen.contains(first_id)
                && screen.contains("activity: ready")
        },
    )?;
    code.pty.wait_for_text("route: direct")?;
    let second = code.pty.checkpoint();
    code.pty.send(b"wait-for-timeout in the new session\r")?;
    code.pty.wait_for_text_since(&second, "FXWAIT")?;
    code.pty.wait_for_text_since(&second, "FXCS")?;
    let cancel = code.mock.wait_for_request("session/cancel")?;
    assert_ne!(cancel["params"]["sessionId"], first["params"]["sessionId"]);
    let captures = std::fs::read_to_string(&code.mock.capture_path)?;
    let requests = captures
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count(),
        2
    );
    assert!(
        !requests
            .iter()
            .any(|r| r["method"] == "session/load" || r["method"] == "session/resume")
    );
    assert_eq!(code.mock.captured_prompts()?.len(), 2);
    // Timeout cancellation finishes cooperatively; closing Code reaps the
    // replacement child and restores the terminal just as the initial launch.
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

#[test]
fn code_operator_restore_runs_through_terminal_review_and_local_publication() -> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let mut config_text = std::fs::read_to_string(&mock.config_path)?;
    config_text.push_str(
        r#"
server:
  skip_auth: true
  control_socket: evolution.sock
database:
  url: "sqlite::memory:"
registry:
  inherit_defaults: false
providers:
  fixture:
    api_base: "http://127.0.0.1:1"
    api_key: fixture-only
    models: [{id: strong}, {id: cheap}]
models:
  coding:
    endpoints: [{provider: fixture, service_id: strong}]
  candidate:
    endpoints: [{provider: fixture, service_id: cheap}]
"#,
    );
    std::fs::write(&mock.config_path, &config_text)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (evolution, _server) = runtime.block_on(async {
        let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
        let (evolution, _, server) = serve_control(&mock, &config).await?;
        evolution
            .register(
                "local",
                BlockDefinition {
                    block_id: "terminal-trial".into(),
                    source: "stub".into(),
                    rationale: "Controlled terminal withdrawal fixture".into(),
                    rules: vec![BlockRule {
                        selector: "coding".into(),
                        fingerprint: None,
                        baseline_route: "coding".into(),
                        challenger_route: "candidate".into(),
                    }],
                    independence_rationale: "One isolated fixture block".into(),
                    dependencies: Default::default(),
                    measurement_contract: "terminal-fixture".into(),
                    batch_sessions: 16,
                    bandit: BanditConfig::default(),
                },
            )
            .await?;
        Ok::<_, anyhow::Error>((evolution, server))
    })?;
    let mut code = CodeFixture::agent_with_mock(mock, None)?;
    code.wait_for_agent_ready()?;
    code.pty.send(b"Complete this controlled coding turn\r")?;
    code.pty.wait_for_text("FXRP1")?;
    code.pty.wait_for_text("Turn completed")?;
    code.pty.send(b"\x10")?;
    code.pty.wait_for_text("Commands")?;
    code.pty.send(b"evolution\r")?;
    code.pty
        .wait_for_text("Checkpoint evaluation and evolution")?;
    code.pty.send(b"Policy block evidence\r")?;
    code.pty.wait_for_text("Policy blocks")?;
    code.pty.send(b"terminal-trial\r")?;
    code.pty.wait_for_text("Original trial recommendation")?;
    code.pty.send(b"\x1b")?;
    code.pty.wait_for_text("Policy block actions")?;
    code.pty.send(b"Restore supported baseline\r")?;
    code.pty.wait_for_text("Why restore this baseline?")?;
    code.pty.paste("Return to the baseline after my review")?;
    code.pty.send(b"\r")?;
    code.pty.wait_for_text("Review baseline restoration")?;
    code.pty.wait_for_text("coding → coding")?;
    code.pty.send(b"\x1b")?;
    code.pty.wait_for_text("Confirm baseline restoration")?;
    code.pty.send(b"Restore this baseline\r")?;
    code.pty.wait_for_text("Withdrawal recorded:")?;
    let state = runtime.block_on(evolution.service("local")?.state())?;
    assert_eq!(state.mode, EvolutionMode::Off);
    assert_eq!(
        state
            .blocks
            .get("terminal-trial")
            .context("block missing")?
            .status,
        BlockStatus::RolledBack
    );
    let receipt = state.publications.last().context("publication missing")?;
    assert_eq!(receipt.action, "operator_restore");
    assert_eq!(
        receipt.operator_reason.as_deref(),
        Some("Return to the baseline after my review")
    );
    assert!(receipt.evidence_digest.is_none());
    code.close_to_composer()?;
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}

async fn fixed_history_score(
    canonical: &CanonicalStore,
    checkpoint: &str,
    name: &str,
    score: u32,
) -> Result<()> {
    use bitrouter::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
    use bitrouter::evolution::{
        evidence::EvidenceKind,
        rubric::{self, Applicability, RubricEvaluation, RubricItem},
        scoring,
    };
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "stub".into(),
        native_session_id: "pty-native".into(),
    };
    let input = scoring::prepare(canonical, &identity, checkpoint).await?;
    let citation = input
        .evidence
        .items
        .iter()
        .find(|item| item.kind == EvidenceKind::AgentMessage)
        .context("captured agent message missing")?
        .citation
        .clone();
    let evaluation = RubricEvaluation {
        rubric_version: rubric::RUBRIC_VERSION.into(),
        items: rubric::library()
            .iter()
            .map(|criterion| RubricItem {
                criterion_id: criterion.id.into(),
                applicability: Applicability::Applicable,
                selection_reason: "Controlled history-display annotation".into(),
                score: if criterion.id == "delivery" {
                    CriterionScore::Scored { value_ppm: score }
                } else {
                    CriterionScore::Unknown
                },
                evidence: if criterion.id == "delivery" {
                    vec![citation.clone()]
                } else {
                    Vec::new()
                },
                explanation: "Fixed fixture score; this does not establish actual task quality."
                    .into(),
            })
            .collect(),
        diagnostics: Vec::new(),
        severe_violation: false,
        violation_evidence: Vec::new(),
        summary: name.into(),
    };
    scoring::submit(
        canonical,
        &identity,
        scoring::RubricSubmission {
            submission_id: name.into(),
            checkpoint_id: checkpoint.into(),
            expected_revision: input.expected_revision,
            source: AssessmentSource::Human,
            evaluator_id: "pty-fixture-reviewer".into(),
            evaluator_version: "fixture-v1".into(),
            evaluation,
        },
    )
    .await?;
    Ok(())
}

#[test]
fn code_checkpoint_history_displays_recorded_revisions_without_changing_the_current_reward()
-> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let database = format!(
        "sqlite://{}?mode=rwc",
        mock._directory.path().join("canonical.db").display()
    );
    let mut config_text = std::fs::read_to_string(&mock.config_path)?;
    config_text.push_str(&format!("\nserver:\n  skip_auth: true\n  control_socket: history.sock\ndatabase:\n  url: {}\nregistry:\n  inherit_defaults: false\nacp_recording:\n  enabled: true\n", serde_json::to_string(&database)?));
    std::fs::write(&mock.config_path, &config_text)?;
    let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (_, canonical, _server) = runtime.block_on(serve_control(&mock, &config))?;
    let mut code = CodeFixture::agent_with_mock(mock, None)?;
    code.wait_for_agent_ready()?;
    code.pty
        .send(b"Record the first controlled coding turn\r")?;
    code.pty.wait_for_text("FXRP1")?;
    code.pty.wait_for_text("Turn completed")?;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "stub".into(),
        native_session_id: "pty-native".into(),
    };
    let first = runtime.block_on(async {
        let head = canonical
            .effective_assessment(&identity)
            .await?
            .current_watermark;
        let checkpoint = canonical.freeze_checkpoint(&identity, head).await?;
        fixed_history_score(
            &canonical,
            &checkpoint.checkpoint_id,
            "Terminal initial annotation",
            1_000_000,
        )
        .await?;
        fixed_history_score(
            &canonical,
            &checkpoint.checkpoint_id,
            "Terminal correction",
            500_000,
        )
        .await?;
        Ok::<_, anyhow::Error>(checkpoint)
    })?;
    let second_turn = code.pty.checkpoint();
    code.pty.send(b"Record a second controlled coding turn\r")?;
    code.pty.wait_for_text_since(&second_turn, "FXRP2")?;
    code.pty.wait_for_screen_inner(
        Some(&second_turn),
        "second recorded turn settled",
        |screen| {
            screen.contains("FXRP2")
                && screen.contains("activity: ready")
                && screen.contains("Turn completed")
        },
    )?;
    let selected = runtime.block_on(async {
        let head = canonical
            .effective_assessment(&identity)
            .await?
            .current_watermark;
        let checkpoint = canonical.freeze_checkpoint(&identity, head).await?;
        assert!(checkpoint.watermark > first.watermark);
        fixed_history_score(
            &canonical,
            &checkpoint.checkpoint_id,
            "Terminal current annotation",
            900_000,
        )
        .await?;
        canonical.effective_assessment(&identity).await
    })?;
    code.pty.send(b"\x10")?;
    code.pty.wait_for_text("Commands")?;
    code.pty.send(b"evolution\r")?;
    code.pty
        .wait_for_text("Checkpoint evaluation and evolution")?;
    code.pty.send(b"Session checkpoints\r")?;
    code.pty
        .wait_for_text("Evaluate the current recorded prefix")?;
    code.pty
        .send(format!("Prefix {}\r", first.watermark).as_bytes())?;
    code.pty.wait_for_text("Manual checkpoint evaluation")?;
    code.pty.wait_for_text("Prefilled from manual")?;
    code.pty.send(b"Evaluation history\r")?;
    code.pty.wait_for_text("Checkpoint evaluation history")?;
    code.pty.send(b"Terminal correction\r")?;
    code.pty.wait_for_text("Stored checkpoint evaluation")?;
    code.pty
        .wait_for_text(&format!("Checkpoint prefix: {}", first.watermark))?;
    code.pty.wait_for_text("pty-fixture-reviewer")?;
    assert_eq!(
        runtime
            .block_on(canonical.effective_assessment(&identity))?
            .current_revision,
        selected.current_revision
    );
    code.close_to_composer()?;
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()
}
