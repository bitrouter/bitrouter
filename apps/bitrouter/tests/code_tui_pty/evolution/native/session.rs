//! Maintained harness sessions and operator CLI, independent of retired Code UI.

use super::*;
use bitrouter::supervisor::{
    ControlFence, ProcessState, SessionCommand, SessionResponse, SessionScope,
};

pub(super) struct NativeFixture {
    pub(super) mock: MockAcp,
    pub(super) api: SessionApi,
    heartbeat: ClientHeartbeat,
}

struct ClientHeartbeat {
    task: tokio::task::JoinHandle<()>,
    error: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl ClientHeartbeat {
    async fn start(api: &SessionApi) -> Result<Self> {
        let lease = api
            .snapshot()
            .await?
            .lease
            .context("native heartbeat lease missing")?;
        ensure!(
            lease.owner_client_id == api.grant.client_id,
            "native lease has a different owner"
        );
        let socket = api.socket.clone();
        let grant = api.grant.clone();
        let run_id = api.run_id.clone();
        let error = Arc::new(tokio::sync::Mutex::new(None));
        let failure = error.clone();
        let task = tokio::spawn(async move {
            loop {
                let response = bitrouter::supervisor::request(
                    &socket,
                    &grant,
                    SessionCommand::Heartbeat {
                        run_id: run_id.clone(),
                        client_id: grant.client_id.clone(),
                        lease_generation: lease.generation,
                    },
                )
                .await;
                match response {
                    Ok(SessionResponse::Lease { lease: confirmed })
                        if confirmed.generation == lease.generation
                            && confirmed.owner_client_id == grant.client_id => {}
                    result => {
                        *failure.lock().await = Some(format!(
                            "native heartbeat lost its original lease: {result:?}"
                        ));
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        Ok(Self { task, error })
    }

    async fn check(&self) -> Result<()> {
        if let Some(error) = &*self.error.lock().await {
            bail!("{error}");
        }
        Ok(())
    }
}

impl Drop for ClientHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NativeFixture {
    pub(super) async fn start(mock: MockAcp, agent: &str) -> Result<Self> {
        let socket = mock.supervisor_socket().await?;
        let grant = bitrouter::supervisor::authorize(
            &socket,
            "maintained-native-session",
            [
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Respond,
                SessionScope::Stop,
            ]
            .into_iter()
            .collect(),
        )
        .await?;
        let response = bitrouter::supervisor::request(
            &socket,
            &grant,
            SessionCommand::Start(Box::new(bitrouter::supervisor::StartRunRequest {
                action_request_id: uuid::Uuid::new_v4().to_string(),
                client_id: Some(grant.client_id.clone()),
                label: Some("maintained native acceptance".into()),
                agent_id: agent.into(),
                prompt: None,
                cwd: mock._directory.path().to_path_buf(),
                routing: bitrouter::acp_cli::RoutingOptions {
                    model: Some(selector(agent).into()),
                    no_start: true,
                    ..Default::default()
                },
                launch: bitrouter::acp_cli::LaunchOptions {
                    turn_timeout: Some(Duration::from_secs(30)),
                    ..Default::default()
                },
                session: Default::default(),
                presentation: bitrouter::supervisor::Presentation::Foreground,
                parent_run_id: None,
                allow_shared_directory: false,
                permission_policy: Default::default(),
                result_schema: None,
            })),
        )
        .await?;
        let SessionResponse::Started { snapshot, .. } = response else {
            bail!("maintained session start was not acknowledged: {response:?}");
        };
        let api = SessionApi {
            socket,
            grant,
            run_id: snapshot.run_id,
        };
        let heartbeat = ClientHeartbeat::start(&api).await?;
        let fixture = Self {
            mock,
            api,
            heartbeat,
        };
        fixture
            .api
            .wait(|snapshot| {
                snapshot.process == ProcessState::Running && snapshot.native_session_id.is_some()
            })
            .await?;
        fixture.explicit_approval(agent).await?;
        Ok(fixture)
    }

    async fn explicit_approval(&self, agent: &str) -> Result<()> {
        if agent == "codex-acp" {
            // Exercise explicit, one-time ACP approval rather than relying on
            // this worker version's separate model-powered auto-reviewer.
            self.api
                .action(
                    &uuid::Uuid::new_v4().to_string(),
                    bitrouter::supervisor::SessionAction::SetConfig {
                        config_id: "mode".into(),
                        value:
                            agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id(
                                "read-only",
                            ),
                    },
                )
                .await?;
            // SetConfig returns the actual complete option table; supervisor
            // installs that response before acknowledging this mutation.
            let settings = self
                .api
                .snapshot()
                .await?
                .initial_settings
                .context("Codex settings missing")?;
            ensure!(
                settings
                    .config_options
                    .is_some_and(|options| options.iter().any(|option| option.id.to_string()
                        == "mode"
                        && matches!(&option.kind,
                    agent_client_protocol::schema::v1::SessionConfigKind::Select(selection)
                        if selection.current_value.to_string() == "read-only"))),
                "Codex config response did not confirm explicit approval mode"
            );
        }
        Ok(())
    }

    pub(super) async fn fresh(&self, agent: &str) -> Result<SessionIdentity> {
        self.heartbeat.check().await?;
        let previous = self.api.snapshot().await?.native_session_id;
        self.api
            .action(
                &uuid::Uuid::new_v4().to_string(),
                bitrouter::supervisor::SessionAction::SelectSession {
                    selection: bitrouter::acp_cli::SessionSelection::New,
                },
            )
            .await?;
        let snapshot = self
            .api
            .wait(|snapshot| {
                snapshot.process == ProcessState::Running
                    && snapshot.native_session_id.is_some()
                    && snapshot.native_session_id != previous
            })
            .await?;
        self.explicit_approval(agent).await?;
        Ok(SessionIdentity {
            owner: "local".into(),
            source: agent.into(),
            native_session_id: snapshot
                .native_session_id
                .context("new native identity missing")?,
        })
    }

    pub(super) async fn prompt(&self, text: &str, expected: &str, coding: bool) -> Result<()> {
        self.heartbeat.check().await?;
        let before = self.api.snapshot().await?.last_seq;
        self.api
            .action(
                &uuid::Uuid::new_v4().to_string(),
                bitrouter::supervisor::SessionAction::Prompt { text: text.into() },
            )
            .await?;
        let mut deadline = tokio::time::Instant::now() + PTY_TIMEOUT;
        let mut permissions = 0;
        loop {
            self.heartbeat.check().await?;
            let snapshot = self.api.snapshot().await?;
            ensure!(
                snapshot.failure.is_none(),
                "native turn failed: {snapshot:?}"
            );
            if let Some(permission) = snapshot.pending_permissions.first() {
                permissions += 1;
                let request = serde_json::to_string(&permission.tool_call)?;
                ensure!(
                    coding
                        && permissions <= 4
                        && (request.contains("rustc") || request.contains("native_sum.rs")),
                    "unexpected native tool permission: {permission:?}"
                );
                let option = permission
                    .options
                    .iter()
                    .find(|option| {
                        serde_json::to_value(option.kind).is_ok_and(|kind| kind == "allow_once")
                    })
                    .context("native coding request omitted allow-once option")?;
                self.api
                    .action(
                        &uuid::Uuid::new_v4().to_string(),
                        bitrouter::supervisor::SessionAction::Permission {
                            permission_id: permission.permission_id.clone(),
                            option_id: option.option_id.to_string(),
                        },
                    )
                    .await?;
                // Each offered approval starts the same bounded next phase as
                // the original four-approval terminal fixture.
                deadline = tokio::time::Instant::now() + PTY_TIMEOUT;
            }
            if snapshot.turn == bitrouter::supervisor::TurnState::Idle {
                let events = self.api.events().await?;
                let new_events = events
                    .events
                    .iter()
                    .filter(|event| event.seq > before)
                    .collect::<Vec<_>>();
                ensure!(
                    new_events.iter().any(|event| matches!(
                        &event.kind,
                        bitrouter::supervisor::SessionEventKind::TurnSettled { .. }
                    )),
                    "native turn omitted its actual settlement"
                );
                ensure!(
                    serde_json::to_string(&new_events)?.contains(expected),
                    "native turn omitted expected reply {expected}: {new_events:?}"
                );
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let events = self.api.events().await?;
                bail!(
                    "native turn did not settle under the original phase deadline: {snapshot:?}; actual events: {:?}",
                    events
                        .events
                        .iter()
                        .filter(|event| event.seq > before)
                        .collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(super) async fn stop(&self) -> Result<()> {
        self.heartbeat.check().await?;
        self.heartbeat.task.abort();
        let lease = self
            .api
            .snapshot()
            .await?
            .lease
            .context("native stop lease missing")?;
        let response = bitrouter::supervisor::request(
            &self.api.socket,
            &self.api.grant,
            SessionCommand::Stop {
                fence: ControlFence {
                    run_id: self.api.run_id.clone(),
                    client_id: self.api.grant.client_id.clone(),
                    lease_generation: lease.generation,
                    action_request_id: uuid::Uuid::new_v4().to_string(),
                },
                confirmed: true,
            },
        )
        .await?;
        ensure!(
            matches!(response, SessionResponse::Action { .. }),
            "native stop was not acknowledged: {response:?}"
        );
        self.api
            .wait(|snapshot| snapshot.process == ProcessState::Stopped)
            .await?;
        Ok(())
    }
}

pub(super) fn cli(
    mock: &MockAcp,
    runtime: &tokio::runtime::Runtime,
    arguments: &[&str],
) -> Result<Value> {
    runtime.block_on(async {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"));
        ensure!(arguments.len() >= 2, "operator CLI requires an ACP command");
        command
            .args(&arguments[..2])
            .args(["--config"])
            .arg(&mock.config_path)
            .args(&arguments[2..])
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", &mock.home_path)
            .env("BITROUTER_HOME", &mock.bitrouter_home_path)
            .env("TMPDIR", &mock.temporary_path)
            .env("NO_PROXY", "localhost,127.0.0.1,::1")
            .env("no_proxy", "localhost,127.0.0.1,::1")
            .current_dir(mock._directory.path())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(PTY_TIMEOUT, command.output())
            .await
            .context("operator CLI exceeded the original deadline")??;
        ensure!(
            output.status.success(),
            "operator CLI {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).context("operator CLI omitted structured receipt")
    })
}

pub(super) fn mode(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    mode: &str,
) -> Result<()> {
    let report = if mode == "automatic" {
        cli(
            &code.mock,
            runtime,
            &["acp", "evolution", "mode", mode, "--judge-model", "judge"],
        )?
    } else {
        cli(&code.mock, runtime, &["acp", "evolution", "mode", mode])?
    };
    ensure!(
        report["report"]["control"]["mode"] == mode,
        "mode receipt did not confirm {mode}: {report}"
    );
    Ok(())
}

pub(super) fn create_candidate(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    agent: &str,
) -> Result<()> {
    mode(code, runtime, "automatic")?;
    let definition = BlockDefinition {
        block_id: "native-terminal-trial".into(),
        source: agent.into(),
        rationale: "Reduce total session cost".into(),
        rules: vec![BlockRule {
            selector: selector(agent).into(),
            fingerprint: None,
            baseline_route: selector(agent).into(),
            challenger_route: "candidate".into(),
        }],
        independence_rationale: "Related changes are in this block".into(),
        dependencies: Default::default(),
        measurement_contract: bitrouter::evolution::scoring::measurement_contract(
            AssessmentSource::Agentic,
            "checkpoint-judge:judge",
            bitrouter::evolution::judge::JUDGE_VERSION,
        )?,
        batch_sessions: 16,
        bandit: BanditConfig::default(),
    };
    let file = code
        .mock
        ._directory
        .path()
        .join("candidate-definition.json");
    std::fs::write(&file, serde_json::to_vec(&definition)?)?;
    let report = cli(
        &code.mock,
        runtime,
        &[
            "acp",
            "evolution",
            "register",
            file.to_str().context("candidate path is not UTF-8")?,
        ],
    )?;
    ensure!(
        report["report"]["control"]["blocks"]["native-terminal-trial"].is_object(),
        "CLI did not register the candidate: {report}"
    );
    Ok(())
}

pub(super) fn inspect(
    code: &NativeFixture,
    runtime: &tokio::runtime::Runtime,
    status: &str,
) -> Result<()> {
    let report = cli(&code.mock, runtime, &["acp", "evolution", "status"])?;
    ensure!(
        report["report"]["control"]["blocks"]["native-terminal-trial"]["status"] == status,
        "actual block status differs from {status}: {report}"
    );
    Ok(())
}

#[test]
fn session_operator_cli_and_fresh_prompt_keep_actual_identity_and_stop() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let mock = MockAcp::new(MockScenario::FreshSessions)?;
    let config = runtime.block_on(bitrouter::paths::load_config(
        &bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
    ))?;
    let (_, _, mut server) = runtime.block_on(serve_control(&mock, &config))?;
    let mut api = runtime.block_on(SessionApi::start(&mock, Some(Duration::from_secs(30))))?;
    api.grant = runtime.block_on(bitrouter::supervisor::authorize(
        &api.socket,
        &api.grant.client_id,
        [
            SessionScope::Peek,
            SessionScope::Respond,
            SessionScope::Stop,
        ]
        .into_iter()
        .collect(),
    ))?;
    let heartbeat = runtime.block_on(ClientHeartbeat::start(&api))?;
    let code = NativeFixture {
        mock,
        api,
        heartbeat,
    };
    mode(&code, &runtime, "off")?;
    runtime.block_on(code.prompt("Explicit first session prompt", "FXRP", false))?;
    let first = runtime.block_on(code.api.snapshot())?.native_session_id;
    let fresh = runtime.block_on(code.fresh("stub"))?;
    assert_ne!(first.as_deref(), Some(fresh.native_session_id.as_str()));
    runtime.block_on(code.prompt("Explicit fresh session prompt", "FXRP", false))?;
    assert_eq!(code.mock.captured_prompts()?.len(), 2);
    runtime.block_on(code.stop())?;
    runtime.block_on(server.stop(&code.api.socket))
}

#[test]
fn heartbeat_preserves_lease_loss_without_reacquiring_or_dispatching() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let config = runtime.block_on(bitrouter::paths::load_config(
        &bitrouter::paths::ConfigSource::File(mock.config_path.clone()),
    ))?;
    let (_, _, mut server) = runtime.block_on(serve_control(&mock, &config))?;
    let api = runtime.block_on(SessionApi::start(&mock, Some(Duration::from_secs(30))))?;
    let heartbeat = runtime.block_on(ClientHeartbeat::start(&api))?;
    let code = NativeFixture {
        mock,
        api,
        heartbeat,
    };
    runtime.block_on(code.api.detach("explicit-lease-release"))?;
    runtime.block_on(async {
        tokio::time::timeout(PTY_TIMEOUT, async {
            loop {
                if code.heartbeat.check().await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("heartbeat did not observe the explicit lease release")
    })?;
    assert!(
        runtime
            .block_on(code.prompt("Must not dispatch after lease release", "FXRP", false))
            .is_err()
    );
    assert!(runtime.block_on(code.api.snapshot())?.lease.is_none());
    assert!(code.mock.captured_prompts()?.is_empty());
    runtime.block_on(server.stop(&code.api.socket))
}
