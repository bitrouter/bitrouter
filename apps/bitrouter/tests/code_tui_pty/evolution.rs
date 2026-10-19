//! Preserved session/evolution APIs against an assembled local control service.
//! Goal terminal acceptance lives in goal_tui_pty. The ACP peer is a fixture;
//! these tests do not evaluate model quality.

use super::*;
use bitrouter::acp_trajectory::{CanonicalStore, SessionIdentity};
use bitrouter::daemon::{AcpControlPlane, NoopObserveStatus, NoopReloader};
use bitrouter::evolution::bandit::BanditConfig;
use bitrouter::evolution::control::{BlockDefinition, BlockRule, BlockStatus, EvolutionMode};

#[path = "evolution/native.rs"]
mod native;

struct ControlServer(tokio::task::JoinHandle<Result<()>>);

impl ControlServer {
    async fn stop(&mut self, socket: &Path) -> Result<()> {
        match bitrouter::daemon::send_command(socket, &bitrouter::daemon::DaemonCommand::Stop)
            .await?
        {
            bitrouter::daemon::DaemonResponse::Ok => {}
            response => bail!("fixture stop was not acknowledged: {response:?}"),
        }
        tokio::time::timeout(PTY_TIMEOUT, &mut self.0).await???;
        Ok(())
    }
}

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
    let administration = bitrouter::actions::administration::Administration {
        source: bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
        routing: assembled.routing_table.clone(),
        policy: assembled.policy_runtime.clone(),
        observe: assembled.observe.clone(),
    };
    let server = ControlServer(tokio::spawn(
        bitrouter::daemon::run_control_socket_with_acp_runtime_and_administration(
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
            Some(administration),
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

struct SessionApi {
    socket: PathBuf,
    grant: bitrouter::supervisor::SessionGrant,
    run_id: String,
}

impl SessionApi {
    async fn start(mock: &MockAcp, turn_timeout: Option<Duration>) -> Result<Self> {
        Self::start_selected(mock, turn_timeout, Default::default()).await
    }

    async fn start_selected(
        mock: &MockAcp,
        turn_timeout: Option<Duration>,
        selection: bitrouter::acp_cli::SessionSelection,
    ) -> Result<Self> {
        use bitrouter::supervisor::{
            Presentation, SessionCommand, SessionResponse, SessionScope, StartRunRequest,
        };
        let socket = mock.supervisor_socket().await?;
        let grant = bitrouter::supervisor::authorize(
            &socket,
            "session-api-migration",
            [
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Respond,
            ]
            .into_iter()
            .collect(),
        )
        .await?;
        let response = bitrouter::supervisor::request(
            &socket,
            &grant,
            SessionCommand::Start(Box::new(StartRunRequest {
                action_request_id: "explicit-session-start".into(),
                client_id: Some(grant.client_id.clone()),
                label: Some("explicit session API fixture".into()),
                agent_id: "stub".into(),
                prompt: None,
                cwd: mock._directory.path().to_path_buf(),
                routing: bitrouter::acp_cli::RoutingOptions {
                    direct: true,
                    ..Default::default()
                },
                launch: bitrouter::acp_cli::LaunchOptions {
                    turn_timeout,
                    ..Default::default()
                },
                session: selection,
                presentation: Presentation::Foreground,
                parent_run_id: None,
                allow_shared_directory: false,
                permission_policy: Default::default(),
                result_schema: None,
            })),
        )
        .await?;
        let SessionResponse::Started { snapshot, .. } = response else {
            bail!("session API omitted the accepted run");
        };
        let api = Self {
            socket,
            grant,
            run_id: snapshot.run_id.clone(),
        };
        api.wait(|snapshot| {
            snapshot.process == bitrouter::supervisor::ProcessState::Running
                && snapshot.native_session_id.is_some()
        })
        .await?;
        Ok(api)
    }

    async fn snapshot(&self) -> Result<bitrouter::supervisor::RunSnapshot> {
        match bitrouter::supervisor::request(
            &self.socket,
            &self.grant,
            bitrouter::supervisor::SessionCommand::Snapshot {
                run_id: self.run_id.clone(),
            },
        )
        .await?
        {
            bitrouter::supervisor::SessionResponse::Snapshot { snapshot } => Ok(*snapshot),
            response => bail!("session API omitted its snapshot: {response:?}"),
        }
    }

    async fn events(&self) -> Result<bitrouter::supervisor::ReplayBatch> {
        use bitrouter::supervisor::{SessionCommand, SessionResponse, SessionScope};
        let grant = bitrouter::supervisor::authorize(
            &self.socket,
            "session-history-reader",
            [SessionScope::Transcript].into_iter().collect(),
        )
        .await?;
        match bitrouter::supervisor::request(
            &self.socket,
            &grant,
            SessionCommand::Events {
                run_id: self.run_id.clone(),
                after_seq: 0,
            },
        )
        .await?
        {
            SessionResponse::Events { replay } => Ok(replay),
            response => bail!("session API omitted history: {response:?}"),
        }
    }

    async fn detach(&self, request_id: &str) -> Result<()> {
        use bitrouter::supervisor::{ControlFence, SessionCommand, SessionResponse};
        let lease = self
            .snapshot()
            .await?
            .lease
            .context("client lease missing")?;
        let response = bitrouter::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::ReleaseLease {
                fence: ControlFence {
                    run_id: self.run_id.clone(),
                    client_id: self.grant.client_id.clone(),
                    lease_generation: lease.generation,
                    action_request_id: request_id.into(),
                },
            },
        )
        .await?;
        ensure!(
            matches!(response, SessionResponse::Action { .. }),
            "detach was not acknowledged: {response:?}"
        );
        Ok(())
    }

    async fn wait(
        &self,
        ready: impl Fn(&bitrouter::supervisor::RunSnapshot) -> bool,
    ) -> Result<bitrouter::supervisor::RunSnapshot> {
        let deadline = tokio::time::Instant::now() + PTY_TIMEOUT;
        loop {
            let snapshot = self.snapshot().await?;
            if ready(&snapshot) {
                return Ok(snapshot);
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "session API did not settle: {snapshot:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn action(
        &self,
        request_id: &str,
        action: bitrouter::supervisor::SessionAction,
    ) -> Result<()> {
        use bitrouter::supervisor::{
            ControlFence, SessionCommand, SessionMutation, SessionResponse,
        };
        let snapshot = self.snapshot().await?;
        let lease = snapshot
            .lease
            .context("session API lost its control lease")?;
        let response = bitrouter::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::Mutate(SessionMutation {
                fence: ControlFence {
                    run_id: self.run_id.clone(),
                    client_id: self.grant.client_id.clone(),
                    lease_generation: lease.generation,
                    action_request_id: request_id.into(),
                },
                action,
            }),
        )
        .await?;
        ensure!(
            matches!(response, SessionResponse::Action { .. }),
            "session mutation was not acknowledged: {response:?}"
        );
        Ok(())
    }

    async fn prompt(&self, request_id: &str, text: &str) -> Result<()> {
        self.action(
            request_id,
            bitrouter::supervisor::SessionAction::Prompt { text: text.into() },
        )
        .await?;
        self.wait(|snapshot| snapshot.turn == bitrouter::supervisor::TurnState::Idle)
            .await?;
        Ok(())
    }
}

async fn evolution_operation(
    socket: &Path,
    operation: bitrouter::evolution::operator::EvolutionOperation,
) -> Result<bitrouter::evolution::operator::EvolutionReport> {
    match bitrouter::daemon::send_command(
        socket,
        &bitrouter::daemon::DaemonCommand::Evolution { operation },
    )
    .await?
    {
        bitrouter::daemon::DaemonResponse::Evolution { report } => Ok(*report),
        bitrouter::daemon::DaemonResponse::Error { message } => bail!("{message}"),
        response => bail!("evolution API omitted its report: {response:?}"),
    }
}

#[tokio::test]
async fn session_api_fresh_session_preserves_direct_routing_and_turn_timeout() -> Result<()> {
    let mock = MockAcp::new(MockScenario::FreshSessions)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, Some(Duration::from_secs(1))).await?;
    api.prompt("first-turn", "First native session").await?;
    let first = mock.wait_for_request("session/prompt")?;
    let first_id = first["params"]["sessionId"]
        .as_str()
        .context("native id missing")?;
    api.action(
        "new-session",
        bitrouter::supervisor::SessionAction::SelectSession {
            selection: bitrouter::acp_cli::SessionSelection::New,
        },
    )
    .await?;
    let fresh = api
        .wait(|snapshot| {
            snapshot
                .native_session_id
                .as_deref()
                .is_some_and(|id| id != first_id)
                && snapshot.process == bitrouter::supervisor::ProcessState::Running
        })
        .await?;
    assert_ne!(fresh.native_session_id.as_deref(), Some(first_id));
    api.action(
        "timeout-turn",
        bitrouter::supervisor::SessionAction::Prompt {
            text: "wait-for-timeout in the new session".into(),
        },
    )
    .await?;
    let failed = api
        .wait(|snapshot| {
            snapshot
                .failure
                .as_deref()
                .is_some_and(|failure| failure.contains("timed out"))
        })
        .await?;
    assert_eq!(
        failed.attention,
        bitrouter::supervisor::AttentionState::Error
    );
    let cancel = mock.wait_for_request("session/cancel")?;
    assert_eq!(
        cancel["params"]["sessionId"].as_str(),
        fresh.native_session_id.as_deref()
    );
    assert_ne!(cancel["params"]["sessionId"].as_str(), Some(first_id));
    let captures = std::fs::read_to_string(&mock.capture_path)?;
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
    assert_eq!(mock.captured_prompts()?.len(), 2);
    assert!(
        fresh.via.is_none(),
        "direct routing unexpectedly used a gateway: {fresh:?}"
    );
    server.stop(&api.socket).await
}

#[tokio::test]
async fn evolution_api_restore_reviews_current_identity_and_retains_publication() -> Result<()> {
    use bitrouter::evolution::control::restoration::RestoreRequest;
    use bitrouter::evolution::operator::{EvolutionOperation, EvolutionReport};
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let mut config_text =
        std::fs::read_to_string(&mock.config_path)?.replace(MOCK_DAEMON_CONFIG, "");
    config_text.push_str(
        r#"
server:
  skip_auth: true
  control_socket: evolution.sock
database:
  url: "sqlite::memory:"
registry:
  enabled: false
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
    let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
    let (evolution, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.prompt("fixture-turn", "Complete this controlled coding turn")
        .await?;
    let registered = evolution_operation(
        &api.socket,
        EvolutionOperation::Register {
            definition: Box::new(BlockDefinition {
                block_id: "terminal-trial".into(),
                source: "stub".into(),
                rationale: "Controlled withdrawal fixture".into(),
                rules: vec![BlockRule {
                    selector: "coding".into(),
                    fingerprint: None,
                    baseline_route: "coding".into(),
                    challenger_route: "candidate".into(),
                }],
                independence_rationale: "One isolated fixture block".into(),
                dependencies: Default::default(),
                measurement_contract: "api-fixture".into(),
                batch_sessions: 16,
                bandit: BanditConfig::default(),
            }),
        },
    )
    .await?;
    let EvolutionReport::Status(registered) = registered else {
        bail!("registration omitted control state");
    };
    let block = registered
        .control
        .blocks
        .get("terminal-trial")
        .context("block missing")?;
    let request = RestoreRequest {
        block: block.definition.block_id.clone(),
        expected_experiment: block.experiment_id.clone(),
        expected_revision: block.revision.clone(),
        reason: "Return to the baseline after my review".into(),
    };
    let generation = registered.control.generation;
    let reviewed = evolution_operation(&api.socket, EvolutionOperation::Status).await?;
    let EvolutionReport::Status(reviewed) = reviewed else {
        bail!("review omitted control state");
    };
    assert_eq!(reviewed.control.generation, generation);
    assert_eq!(mock.captured_prompts()?.len(), 1);
    let stale = RestoreRequest {
        expected_revision: "outdated-review".into(),
        ..request.clone()
    };
    assert!(
        evolution_operation(&api.socket, EvolutionOperation::Restore { request: stale })
            .await
            .is_err()
    );
    assert_eq!(
        evolution.service("local")?.state().await?.generation,
        generation
    );
    evolution_operation(
        &api.socket,
        EvolutionOperation::Restore {
            request: request.clone(),
        },
    )
    .await?;
    evolution_operation(&api.socket, EvolutionOperation::Restore { request }).await?;
    let state = evolution.service("local")?.state().await?;
    assert_eq!(state.mode, EvolutionMode::Off);
    assert_eq!(
        state
            .blocks
            .get("terminal-trial")
            .context("block missing")?
            .status,
        BlockStatus::RolledBack
    );
    assert_eq!(state.generation, generation + 1);
    assert_eq!(state.publications.len(), 1);
    let receipt = state.publications.last().context("publication missing")?;
    assert_eq!(receipt.action, "operator_restore");
    assert_eq!(
        receipt.operator_reason.as_deref(),
        Some("Return to the baseline after my review")
    );
    assert!(receipt.evidence_digest.is_none());
    assert_eq!(
        mock.captured_prompts()?.len(),
        1,
        "review or restore dispatched another turn"
    );
    server.stop(&api.socket).await
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

#[tokio::test]
async fn evolution_api_checkpoint_history_keeps_current_reward_and_original_revisions() -> Result<()>
{
    use bitrouter::evolution::operator::checkpoint::{CheckpointAction, CheckpointReport};
    use bitrouter::evolution::operator::{EvolutionOperation, EvolutionReport};
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let database = format!(
        "sqlite://{}?mode=rwc",
        mock._directory.path().join("canonical.db").display()
    );
    let mut config_text =
        std::fs::read_to_string(&mock.config_path)?.replace(MOCK_DAEMON_CONFIG, "");
    config_text.push_str(&format!(
        "\nserver:\n  skip_auth: true\n  control_socket: history.sock\ndatabase:\n  url: {}\nregistry:\n  enabled: false\nacp_recording:\n  enabled: true\n",
        serde_json::to_string(&database)?
    ));
    std::fs::write(&mock.config_path, &config_text)?;
    let config = bitrouter_sdk::config::parse_with(&config_text, |_| None)?;
    let (_, canonical, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.prompt(
        "first-recorded-turn",
        "Record the first controlled coding turn",
    )
    .await?;
    let identity = SessionIdentity {
        owner: "local".into(),
        source: "stub".into(),
        native_session_id: "pty-native".into(),
    };
    let first_head = canonical
        .effective_assessment(&identity)
        .await?
        .current_watermark;
    let first = canonical.freeze_checkpoint(&identity, first_head).await?;
    fixed_history_score(
        &canonical,
        &first.checkpoint_id,
        "Terminal initial annotation",
        1_000_000,
    )
    .await?;
    fixed_history_score(
        &canonical,
        &first.checkpoint_id,
        "Terminal correction",
        500_000,
    )
    .await?;
    api.prompt(
        "second-recorded-turn",
        "Record a second controlled coding turn",
    )
    .await?;
    let head = canonical
        .effective_assessment(&identity)
        .await?
        .current_watermark;
    let current = canonical.freeze_checkpoint(&identity, head).await?;
    assert!(current.watermark > first.watermark);
    fixed_history_score(
        &canonical,
        &current.checkpoint_id,
        "Terminal current annotation",
        900_000,
    )
    .await?;
    let selected = canonical.effective_assessment(&identity).await?;
    let report = evolution_operation(
        &api.socket,
        EvolutionOperation::Checkpoint {
            source: identity.source.clone(),
            session_id: identity.native_session_id.clone(),
            action: CheckpointAction::Review {
                checkpoint_id: first.checkpoint_id.clone(),
            },
        },
    )
    .await?;
    let EvolutionReport::Checkpoint(report) = report else {
        bail!("checkpoint API omitted its report");
    };
    let CheckpointReport::Review(review) = *report else {
        bail!("checkpoint API omitted revision history");
    };
    assert_eq!(review.identity, identity);
    assert_eq!(review.checkpoint.checkpoint_id, first.checkpoint_id);
    assert_eq!(review.checkpoint.watermark, first.watermark);
    assert_eq!(review.history.len(), 2);
    assert_eq!(
        review.prefill_revision.as_ref(),
        review.history.last().map(|revision| &revision.revision_id)
    );
    assert_eq!(
        review
            .previous
            .as_ref()
            .map(|evaluation| evaluation.summary.as_str()),
        Some("Terminal correction")
    );
    assert_eq!(review.expected_revision, selected.current_revision);
    assert!(
        review
            .history
            .iter()
            .all(|revision| revision.input.checkpoint_id == first.checkpoint_id)
    );
    let after = canonical.effective_assessment(&identity).await?;
    assert_eq!(after.current_revision, selected.current_revision);
    assert_eq!(after.current_watermark, selected.current_watermark);
    assert_eq!(
        mock.captured_prompts()?.len(),
        2,
        "history query dispatched another turn"
    );
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_settings_apply_only_the_actual_confirmed_configuration() -> Result<()> {
    let mock = MockAcp::new(MockScenario::SettingsConfirmed)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    let before = api.snapshot().await?;
    let old = before
        .initial_settings
        .context("initial settings missing")?
        .config_options
        .context("config options missing")?;
    assert_eq!(serde_json::to_value(old)?[0]["currentValue"], "a12-old");
    api.action(
        "confirmed-setting",
        bitrouter::supervisor::SessionAction::SetConfig {
            config_id: "a12-setting".into(),
            value: agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(
                "a12-confirmed",
            ),
        },
    )
    .await?;
    let request = mock.wait_for_request("session/set_config_option")?;
    assert_eq!(request["params"]["sessionId"], "pty-native");
    assert_eq!(request["params"]["configId"], "a12-setting");
    assert_eq!(request["params"]["value"], "a12-confirmed");
    let after = api.snapshot().await?;
    let options = after
        .initial_settings
        .context("confirmed settings missing")?
        .config_options
        .context("confirmed options missing")?;
    assert_eq!(
        serde_json::to_value(options)?[0]["currentValue"],
        "a12-confirmed"
    );
    assert!(
        mock.captured_prompts()?.is_empty(),
        "settings inspection dispatched a prompt"
    );
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_failed_settings_retain_the_last_confirmed_value() -> Result<()> {
    let mock = MockAcp::new(MockScenario::SettingsFailure)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    let response = api
        .action(
            "failed-setting",
            bitrouter::supervisor::SessionAction::SetConfig {
                config_id: "a12-setting".into(),
                value: agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(
                    "a12-confirmed",
                ),
            },
        )
        .await;
    assert!(response.is_err());
    let error = response.err().context("setting unexpectedly succeeded")?;
    ensure!(
        error.to_string().contains("FXSF"),
        "wrong setting rejection: {error}"
    );
    let request = mock.wait_for_request("session/set_config_option")?;
    assert_eq!(request["params"]["sessionId"], "pty-native");
    assert_eq!(request["params"]["configId"], "a12-setting");
    assert_eq!(request["params"]["value"], "a12-confirmed");
    let after = api.snapshot().await?;
    let options = after
        .initial_settings
        .context("last confirmed settings missing")?
        .config_options
        .context("options missing")?;
    assert_eq!(serde_json::to_value(options)?[0]["currentValue"], "a12-old");
    assert!(mock.captured_prompts()?.is_empty());
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_load_replays_history_and_resume_preserves_distinct_native_ids() -> Result<()> {
    for (selection, native_id, agent_id, method, replayed) in [
        (
            bitrouter::acp_cli::SessionSelection::Load("native-a12-load".into()),
            "native-a12-load",
            "agent-a12-load",
            "session/load",
            true,
        ),
        (
            bitrouter::acp_cli::SessionSelection::Resume("native-a12-resume".into()),
            "native-a12-resume",
            "agent-a12-resume",
            "session/resume",
            false,
        ),
    ] {
        let mock = MockAcp::new(MockScenario::SessionLifecycle)?;
        let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
            mock.config_path.clone(),
        ))
        .await?;
        let (_, _, mut server) = serve_control(&mock, &config).await?;
        let api = SessionApi::start_selected(&mock, None, selection).await?;
        let snapshot = api.snapshot().await?;
        assert_eq!(snapshot.native_session_id.as_deref(), Some(native_id));
        assert_eq!(snapshot.agent_session_id.as_deref(), Some(agent_id));
        let request = mock.wait_for_request(method)?;
        assert_eq!(request["params"]["sessionId"], native_id);
        assert!(
            mock.captured_request(if replayed {
                "session/resume"
            } else {
                "session/load"
            })?
            .is_none()
        );
        let history = api.events().await?;
        ensure!(history.history_complete && history.first_retained_seq == 1);
        let text = serde_json::to_string(&history.events)?;
        assert_eq!(
            text.contains("FXLOAD"),
            replayed,
            "native history replay changed: {text}"
        );
        assert!(
            mock.captured_prompts()?.is_empty(),
            "loading history dispatched a new prompt"
        );
        server.stop(&api.socket).await?;
    }
    Ok(())
}

#[tokio::test]
async fn session_api_overlapping_permissions_keep_identity_and_require_explicit_answers()
-> Result<()> {
    use bitrouter::supervisor::{SessionAction, TurnState};
    let mock = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.action(
        "permission-turn",
        SessionAction::Prompt {
            text: "permission test".into(),
        },
    )
    .await?;
    let pending = api
        .wait(|snapshot| snapshot.pending_permissions.len() == 2)
        .await?;
    assert_eq!(pending.turn, TurnState::Working);
    assert!(mock.captured_permission_outcomes()?.is_empty());
    for (index, request) in pending.pending_permissions.iter().enumerate() {
        assert_eq!(
            request.tool_call.tool_call_id.0.as_ref(),
            format!("tool-{}", index + 1)
        );
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.name.as_str())
                .collect::<Vec<_>>(),
            [
                format!("Allow fixture {}", index + 1),
                format!("Deny fixture {}", index + 1)
            ]
        );
    }
    let first = &pending.pending_permissions[0];
    let second = &pending.pending_permissions[1];
    api.action(
        "first-explicit-answer",
        SessionAction::Permission {
            permission_id: first.permission_id.clone(),
            option_id: first.options[0].option_id.0.to_string(),
        },
    )
    .await?;
    let remaining = api
        .wait(|snapshot| snapshot.pending_permissions.len() == 1)
        .await?;
    assert_eq!(
        remaining.pending_permissions[0].permission_id,
        second.permission_id
    );
    assert_eq!(remaining.turn, TurnState::Working);
    let outcomes = mock.wait_for_permission_outcomes(1)?;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1["optionId"], "allow-1");
    api.action(
        "second-explicit-answer",
        SessionAction::Permission {
            permission_id: second.permission_id.clone(),
            option_id: second.options[0].option_id.0.to_string(),
        },
    )
    .await?;
    api.wait(|snapshot| {
        snapshot.turn == TurnState::Idle && snapshot.pending_permissions.is_empty()
    })
    .await?;
    let outcomes = mock.wait_for_permission_outcomes(2)?;
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .any(|(_, outcome)| outcome["optionId"] == "allow-1")
    );
    assert!(
        outcomes
            .iter()
            .any(|(_, outcome)| outcome["optionId"] == "allow-2")
    );
    assert_eq!(mock.captured_prompts()?, vec!["permission test"]);
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_late_adapter_failure_keeps_completed_transcript_and_native_identity()
-> Result<()> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.prompt("completed-turn", "completed before disconnect")
        .await?;
    let completed = api.snapshot().await?;
    ensure!(serde_json::to_string(&api.events().await?.events)?.contains("FXRP1"));
    mock.disconnect()?;
    let failed = api
        .wait(|snapshot| snapshot.process == bitrouter::supervisor::ProcessState::Failed)
        .await?;
    assert_eq!(failed.native_session_id, completed.native_session_id);
    assert_eq!(failed.turn, bitrouter::supervisor::TurnState::Idle);
    assert!(failed.failure.is_some());
    let history = api.events().await?;
    ensure!(history.history_complete && history.first_retained_seq == 1);
    ensure!(
        serde_json::to_string(&history.events)?.contains("FXRP1"),
        "completed transcript was lost"
    );
    assert_eq!(
        mock.captured_prompts()?,
        vec!["completed before disconnect"]
    );
    server.stop(&api.socket).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_api_client_loss_during_startup_keeps_daemon_ownership_until_explicit_stop()
-> Result<()> {
    use bitrouter::supervisor::{
        ControlFence, LeaseMode, ProcessState, SessionCommand, SessionResponse, SessionScope,
    };
    let mock = Arc::new(MockAcp::new(MockScenario::HangingSession)?);
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let socket = mock.supervisor_socket().await?;
    let reader = bitrouter::supervisor::authorize(
        &socket,
        "startup-observer",
        [SessionScope::Peek].into_iter().collect(),
    )
    .await?;
    let startup = {
        let mock = mock.clone();
        tokio::spawn(async move { SessionApi::start(&mock, None).await })
    };
    // Start only replies after session/new settles. Observe the persisted
    // Starting run independently, then drop the caller's live IPC request.
    tokio::time::timeout(PTY_TIMEOUT, async {
        while mock.captured_request("session/new")?.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let SessionResponse::Runs { runs } =
        bitrouter::supervisor::request(&socket, &reader, SessionCommand::PeekAll).await?
    else {
        bail!("startup observer did not return runs");
    };
    ensure!(
        runs.len() == 1,
        "startup registered an unexpected run count"
    );
    let starting = runs.into_iter().next().context("starting run missing")?;
    assert_eq!(starting.process, ProcessState::Starting);
    let owner = starting.lease.context("startup client lease missing")?;
    let api = SessionApi {
        socket,
        grant: reader,
        run_id: starting.run_id,
    };
    startup.abort();
    let Err(lost) = startup.await else {
        bail!("startup caller unexpectedly returned");
    };
    ensure!(lost.is_cancelled(), "startup caller was not cancelled");
    let detached = api.snapshot().await?;
    assert_eq!(detached.process, ProcessState::Starting);
    assert_eq!(
        detached
            .lease
            .context("client loss removed the lease")?
            .owner_client_id,
        owner.owner_client_id
    );
    assert!(mock.captured_request("session/cancel")?.is_none());
    let grant = bitrouter::supervisor::authorize(
        &api.socket,
        "startup-recovery",
        [SessionScope::Stop].into_iter().collect(),
    )
    .await?;
    let SessionResponse::Lease { lease } = bitrouter::supervisor::request(
        &api.socket,
        &grant,
        SessionCommand::AcquireLease {
            run_id: api.run_id.clone(),
            client_id: grant.client_id.clone(),
            mode: LeaseMode::Transient,
            action_request_id: "confirmed-startup-takeover".into(),
            takeover: true,
        },
    )
    .await?
    else {
        bail!("takeover did not return a lease");
    };
    tokio::time::timeout(
        PTY_TIMEOUT,
        bitrouter::supervisor::request(
            &api.socket,
            &grant,
            SessionCommand::Stop {
                fence: ControlFence {
                    run_id: api.run_id.clone(),
                    client_id: grant.client_id.clone(),
                    lease_generation: lease.generation,
                    action_request_id: "confirmed-startup-stop".into(),
                },
                confirmed: true,
            },
        ),
    )
    .await??;
    assert_eq!(api.snapshot().await?.process, ProcessState::Stopped);
    ensure!(
        UnixStream::connect(&mock.control_path).is_err(),
        "startup adapter was not reaped"
    );
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_detach_preserves_pending_permissions_without_answering_or_stopping()
-> Result<()> {
    let mock = MockAcp::new(MockScenario::OverlappingPermissions)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.action(
        "pending-permissions",
        bitrouter::supervisor::SessionAction::Prompt {
            text: "permission interrupted by client loss".into(),
        },
    )
    .await?;
    let before = api
        .wait(|snapshot| snapshot.pending_permissions.len() == 2)
        .await?;
    api.detach("permission-client-detached").await?;
    let after = api.snapshot().await?;
    assert_eq!(after.process, bitrouter::supervisor::ProcessState::Running);
    assert_eq!(after.turn, bitrouter::supervisor::TurnState::Working);
    assert!(after.lease.is_none());
    assert_eq!(
        serde_json::to_value(after.pending_permissions)?,
        serde_json::to_value(before.pending_permissions)?
    );
    assert!(mock.captured_permission_outcomes()?.is_empty());
    assert!(mock.captured_request("session/cancel")?.is_none());
    server.stop(&api.socket).await
}

#[tokio::test]
async fn session_api_explicit_detach_keeps_accepted_turn_running_and_delivers_after_client_exit()
-> Result<()> {
    let mock = MockAcp::new(MockScenario::DelayedNormal)?;
    let config = bitrouter::paths::load_config(&bitrouter::paths::ConfigSource::File(
        mock.config_path.clone(),
    ))
    .await?;
    let (_, _, mut server) = serve_control(&mock, &config).await?;
    let api = SessionApi::start(&mock, None).await?;
    api.action(
        "accepted-turn",
        bitrouter::supervisor::SessionAction::Prompt {
            text: "continue after explicit detach".into(),
        },
    )
    .await?;
    let active = api
        .wait(|snapshot| snapshot.turn == bitrouter::supervisor::TurnState::Working)
        .await?;
    api.detach("explicit-client-detach").await?;
    let detached = api.snapshot().await?;
    assert_eq!(
        detached.process,
        bitrouter::supervisor::ProcessState::Running
    );
    assert_eq!(detached.turn, bitrouter::supervisor::TurnState::Working);
    assert_eq!(detached.native_session_id, active.native_session_id);
    assert!(detached.lease.is_none());
    assert!(mock.captured_request("session/cancel")?.is_none());
    mock.release()?;
    let delivered = api
        .wait(|snapshot| {
            snapshot.turn == bitrouter::supervisor::TurnState::Idle
                && snapshot.review == bitrouter::supervisor::ReviewState::Unread
        })
        .await?;
    assert_eq!(delivered.native_session_id, active.native_session_id);
    assert!(delivered.lease.is_none());
    assert_eq!(
        mock.captured_prompts()?,
        vec!["continue after explicit detach"]
    );
    server.stop(&api.socket).await
}
