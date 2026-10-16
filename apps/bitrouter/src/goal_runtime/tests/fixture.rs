//! Concrete transport fixture: real ACP processes, daemon IPC and SQLite state.
use super::*;

const OBJECTIVE: &str = "Deliver LEFT and RIGHT files in parallel, then join their actual contents. BRO must verify all three files. No human acceptance is required.";

pub(super) struct Setup {
    temporary: tempfile::TempDir,
    pub workspace: PathBuf,
    pub controls: PathBuf,
    pub captures: PathBuf,
    pub backend: MockServer,
    config_text: String,
    socket: Option<PathBuf>,
}

pub(super) struct Fixture {
    setup: Setup,
    pub socket: PathBuf,
    pub supervisor: Supervisor,
    pub source: crate::paths::ConfigSource,
    pub routing: Arc<ConfigRoutingTable>,
    pub threads: bitrouter_orchestrator::service::ThreadService,
    pub goals: Arc<GoalService>,
    pub db: sea_orm::DatabaseConnection,
    runtime: GoalRuntime,
    ipc: JoinHandle<Result<()>>,
}

impl std::ops::Deref for Fixture {
    type Target = Setup;
    fn deref(&self) -> &Self::Target {
        &self.setup
    }
}

impl Setup {
    pub async fn new() -> Result<Self> {
        Self::build(true).await
    }

    pub async fn new_without_external_harnesses() -> Result<Self> {
        Self::build(false).await
    }

    async fn build(with_harness: bool) -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path();
        let workspace = home.join("original");
        let controls = home.join("controls");
        let captures = home.join("captures");
        for path in [&workspace, &controls, &captures] {
            tokio::fs::create_dir_all(path).await?;
        }
        tokio::fs::write(workspace.join("seed.txt"), "original input\n").await?;
        git(&workspace, &["init".as_ref(), "-q".as_ref()]).await?;
        git(&workspace, &["add".as_ref(), "seed.txt".as_ref()]).await?;
        git(
            &workspace,
            &[
                "-c".as_ref(),
                "user.name=Goal Fixture".as_ref(),
                "-c".as_ref(),
                "user.email=goal-fixture@example.test".as_ref(),
                "-c".as_ref(),
                "core.hooksPath=/dev/null".as_ref(),
                "commit".as_ref(),
                "--no-gpg-sign".as_ref(),
                "-qm".as_ref(),
                "fixture baseline".as_ref(),
            ],
        )
        .await?;
        let backend = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(provider::model)
            .with_priority(10)
            .mount(&backend)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(provider::typed)
            .mount(&backend)
            .await;
        let harness = if with_harness {
            format!(
                "agents:\n  goal-peer:\n    name: goal-peer\n    transport:\n      type: stdio\n      command: {}\n      args: []\n      env:\n        BRO_GOAL_FIXTURE_CONTROL: {}\n        BRO_GOAL_FIXTURE_CAPTURE: {}\n",
                serde_json::to_string(&peer_executable()?)?,
                serde_json::to_string(&controls)?,
                serde_json::to_string(&captures)?
            )
        } else {
            String::new()
        };
        let config_text = format!(
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '127.0.0.1:0', skip_auth: true}}\ndatabase: {{url: 'sqlite::memory:'}}\nchat: {{model: goal-model}}\nproviders:\n  fixture:\n    api_base: {}/v1\n    api_key: goal-fixture-key\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\nmodels:\n  goal-model:\n    endpoints: [{{provider: fixture, service_id: test-model}}]\ndecision_model:\n  model: fixture-input\n  base_url: {}\n  api_key_env: USER\n{harness}",
            backend.uri(),
            backend.uri()
        );
        let socket = Some(home.join("goal.sock"));
        let setup = Self {
            temporary,
            workspace,
            controls,
            captures,
            backend,
            config_text,
            socket,
        };
        Ok(setup)
    }

    pub fn agent_env(&mut self, name: &str) {
        self.config_text = self.config_text.replacen(
            "      env:\n",
            &format!("      env:\n        {name}: '1'\n"),
            1,
        );
    }

    pub fn persistent_database(&mut self) -> Result<()> {
        let url = format!(
            "sqlite://{}?mode=rwc",
            self.temporary.path().join("runtime.sqlite").display()
        );
        let configured = format!("database: {{url: {}}}", serde_json::to_string(&url)?);
        let memory = "database: {url: 'sqlite::memory:'}";
        if self.config_text.contains(memory) {
            self.config_text = self.config_text.replacen(memory, &configured, 1);
        } else {
            anyhow::ensure!(
                self.config_text.contains(&configured),
                "fixture has another database configuration"
            );
        }
        Ok(())
    }

    pub fn native_fallback(&mut self) {
        self.config_text = self.config_text.replacen(
            "\nmodels:\n",
            &format!("\n  fixture-backup:\n    api_base: {}/v2\n    api_key: backup-fixture-key\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\nmodels:\n", self.backend.uri()),
            1,
        );
        self.config_text = self.config_text.replacen(
            "endpoints: [{provider: fixture, service_id: test-model}]",
            "endpoints: [{provider: fixture, service_id: test-model}, {provider: fixture-backup, service_id: test-model}]",
            1,
        );
    }

    pub fn worker_alias(&mut self, id: &str) -> Result<()> {
        let config = bitrouter_sdk::config::parse_with(&self.config_text, |_| None)?;
        let mut agent = config
            .agents
            .get("goal-peer")
            .cloned()
            .context("fixture peer missing")?;
        agent.name = id.into();
        let bitrouter_sdk::config::agent::AcpTransport::Stdio { env, .. } = &mut agent.transport;
        env.insert("BRO_GOAL_FIXTURE_REQUIRE_HANDOFF_LEFT".into(), "1".into());
        let (_, section) = self
            .config_text
            .split_once("\nagents:\n")
            .context("fixture agent block missing")?;
        anyhow::ensure!(
            section
                .lines()
                .all(|line| line.is_empty() || line.starts_with("  ")),
            "worker alias requires the final fixture agent block"
        );
        self.config_text.push_str(&format!(
            "  {}: {}\n",
            serde_json::to_string(id)?,
            serde_json::to_string(&agent)?
        ));
        Ok(())
    }

    pub fn review_api(&mut self, uri: &str) {
        self.config_text = self.config_text.replace(
            &format!("api_base: {}/v1", self.backend.uri()),
            &format!("api_base: {uri}/v1"),
        );
    }

    pub async fn routed_agent(&mut self) {
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&self.backend)
            .await;
        let listen = self.backend.uri().trim_start_matches("http://").to_owned();
        self.config_text = self.config_text.replace("127.0.0.1:0", &listen).replace(
            "args: []",
            "args: ['@agentclientprotocol/codex-acp@1.10.0']",
        );
        self.socket = None;
    }

    pub async fn resources(&mut self) -> Result<()> {
        let skill_dir = self.workspace.join("skills/goal-resource-fixture");
        tokio::fs::create_dir_all(&skill_dir).await?;
        tokio::fs::write(skill_dir.join("SKILL.md"), "---\nname: goal-resource-fixture\ndescription: Fixture material for selected context\n---\nGOAL_RESOURCE_SKILL_MARKER: report actual verification.\n").await?;
        let mcp = fixture_binary::build("harness_mcp_fixture").map_err(anyhow::Error::msg)?;
        self.config_text.push_str(&format!(
            "mcp_servers:\n  selected-resource-server:\n    name: selected-resource-server\n    transport:\n      type: stdio\n      command: {}\n      env: {{RESOURCE_TOKEN: 'private-mcp-fixture-token'}}\n  unselected-resource-server:\n    name: unselected-resource-server\n    transport:\n      type: stdio\n      command: {}\n",
            serde_json::to_string(&mcp)?, serde_json::to_string(&mcp)?
        ));
        Ok(())
    }

    pub async fn start(self) -> Result<Fixture> {
        let home = self.temporary.path();
        let config_path = home.join("bitrouter.yaml");
        tokio::fs::write(&config_path, &self.config_text).await?;
        let config = bitrouter_sdk::config::parse_with(&self.config_text, |_| None)?;
        let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
        let app = Arc::new(assembled.app);
        let goals = Arc::new(GoalService::new(Arc::new(
            crate::goal_store::DatabaseGoalStore::new(assembled.db.clone()),
        )));
        let source = crate::paths::ConfigSource::File(config_path);
        let fixture_db = assembled.db.clone();
        let socket = match &self.socket {
            Some(socket) => socket.clone(),
            None => crate::daemon::socket_path_for(&source, &config),
        };
        let routing = Arc::new(ConfigRoutingTable::from_config(config));
        let supervisor = Supervisor::open(source.clone(), routing.clone()).await?;
        let gate = crate::daemon_handoff::HandoffGate::default();
        let threads = bitrouter_orchestrator::service::ThreadService::with_store(
            app.clone(),
            &[],
            Arc::new(crate::agent_store::DatabaseExecutionStore::new(
                assembled.db.clone(),
            )),
        )?;
        let runtime = GoalRuntime::start(
            app.clone(),
            goals.clone(),
            threads.clone(),
            supervisor.clone(),
            routing.clone(),
            home,
            gate.clone(),
        )
        .await?;
        let bound = bind_control_socket(&socket).await?;
        let ipc = tokio::spawn(run_bound_control_socket(
            bound,
            app,
            "127.0.0.1:0".into(),
            Arc::new(NoopReloader),
            Arc::new(NoopObserveStatus { compiled_in: false }),
            AcpControlPlane {
                runtime: assembled.acp_runtime,
                metering: crate::metering::MeteringStore::new(assembled.db),
                inventory: None,
                evolution: None,
            },
            LocalControlOptions {
                administration: None,
                threads: None,
                goals: Some(goals.clone()),
                gate,
                cli_owned: false,
            },
        ));
        Ok(Fixture {
            setup: self,
            socket,
            supervisor,
            source,
            routing,
            threads,
            goals,
            db: fixture_db,
            runtime,
            ipc,
        })
    }
}

impl Fixture {
    pub fn root(&self, id: &str) -> PathBuf {
        crate::goal_artifacts::root(&self.temporary.path().join("goal-workspaces"), id)
    }

    pub async fn create(&self) -> Result<String> {
        self.create_with(OBJECTIVE).await
    }

    pub async fn create_with(&self, objective: &str) -> Result<String> {
        let GoalReply::Accepted { operation } = rpc(
            &self.socket,
            GoalCommand::Create {
                operation_id: "wire-goal".into(),
                title: "Fixture parallel delivery".into(),
                workspace: self.workspace.clone(),
                objective: objective.into(),
                budget_limits: Default::default(),
            },
        )
        .await?
        else {
            anyhow::bail!("creation omitted its receipt")
        };
        Ok(operation.goal_id)
    }

    pub async fn running(&self, id: &str) -> Result<GoalSnapshot> {
        let running = wait_for(&self.socket, id, |goal| {
            goal.workflow
                .tasks
                .iter()
                .filter(|task| {
                    task.executions.last().is_some_and(|execution| {
                        matches!(execution.observation, Observation::Running { .. })
                    })
                })
                .count()
                == 2
                && self.controls.join("started-left").exists()
                && self.controls.join("started-right").exists()
        })
        .await?;
        assert_eq!(running.requirements_revision, 1);
        assert!(self.controls.join("started-left").exists());
        assert!(self.controls.join("started-right").exists());
        assert!(!self.controls.join("started-join").exists());
        Ok(running)
    }

    pub async fn input(
        &self,
        id: &str,
        operation: &str,
        text: &str,
        task: Option<&str>,
    ) -> Result<()> {
        for _ in 0..8 {
            let current = head(&self.socket, id).await?;
            match rpc(
                &self.socket,
                GoalCommand::Input {
                    operation_id: operation.into(),
                    goal_id: id.into(),
                    expected_revision: current.revision,
                    text: text.into(),
                    task_reference: task.map(str::to_owned),
                },
            )
            .await
            {
                Ok(GoalReply::Accepted { .. }) => return Ok(()),
                Err(error)
                    if error.downcast_ref::<bitrouter_orchestrator::goal::GoalError>()
                        == Some(&bitrouter_orchestrator::goal::GoalError::Conflict) => {}
                other => anyhow::bail!("fixture input was not admitted: {other:?}"),
            }
        }
        anyhow::bail!("fixture input remained in conflict")
    }

    pub async fn release(&self) -> Result<()> {
        tokio::fs::write(self.controls.join("release"), "release fixture only").await?;
        Ok(())
    }

    pub async fn completed(&self, id: &str, version: u64) -> Result<GoalSnapshot> {
        let completed = wait_for(&self.socket, id, |goal| {
            goal.workflow.completion.completed_for(version)
        })
        .await?;
        let delivery = completed
            .workflow
            .completion
            .last_completed
            .as_ref()
            .context("missing verified delivery")?;
        assert_eq!(delivery.requirements_revision, version);
        assert!(!self.workspace.join("left.txt").exists());
        assert!(!self.workspace.join("right.txt").exists());
        assert!(self.controls.join("started-join").exists());
        let root = self.root(id);
        let joined = completed
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "join")
            .and_then(|task| task.executions.last())
            .context("joined execution missing")?;
        let prepared = joined.resources.as_ref().context("joined input missing")?;
        let reference = prepared
            .checked_dependencies
            .as_ref()
            .context("dependency reports were not frozen before dispatch")?;
        let supplied = crate::goal_artifacts::read(&root, reference).await?;
        let dependencies: serde_json::Value = serde_json::from_slice(&supplied)?;
        assert_eq!(
            dependencies
                .as_array()
                .context("dependencies not an array")?
                .len(),
            2
        );
        let prompt =
            String::from_utf8(crate::goal_artifacts::read(&root, &prepared.prompt).await?)?;
        assert!(prompt.contains(std::str::from_utf8(&supplied)?));
        let material = crate::goal_artifacts::material(&root, &delivery.delivery).await?;
        let material = serde_json::to_string(&material)?;
        assert!(
            material.contains("left.txt")
                && material.contains("right.txt")
                && material.contains("joined.txt")
        );
        assert!(completed.workflow.tasks.iter().all(|task| {
            task.executions.last().is_some_and(|execution| {
                execution.handoff.is_some()
                    && matches!(execution.stop, Some(StopState::Confirmed { .. }))
            })
        }));
        Ok(completed)
    }

    pub async fn left_history(&self) -> Result<serde_json::Value> {
        let requests = self
            .backend
            .received_requests()
            .await
            .context("fixture request capture unavailable")?;
        let assessment = requests
            .iter()
            .filter_map(provider::model_state)
            .find(|state| state.get("integrated_material").is_some())
            .context("final evidence request missing")?;
        let left = assessment["task_evidence"]
            .as_array()
            .context("task evidence missing")?
            .iter()
            .find(|task| task["task"]["id"] == "left")
            .context("left history missing")?;
        let history = &left["historical_runtime_observations"];
        assert_eq!(history["historical_only"], true);
        assert_eq!(history["current_artifact_authority"], false);
        assert!(history["entries"].as_array().is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry["status"] == "observed" && entry["admission_is_execution_evidence"] == false
            })
        }));
        Ok(history.clone())
    }

    async fn failure_runtime(&self) -> Result<serde_json::Value> {
        let GoalReply::Directory { page } = self
            .goals
            .execute(
                "local",
                GoalCommand::List {
                    after: 0,
                    cutoff: None,
                    limit: bitrouter_orchestrator::goal::MAX_PAGE_ITEMS,
                },
                now()?,
            )
            .await?
        else {
            anyhow::bail!("fixture Goal directory unavailable")
        };
        let mut executions = Vec::new();
        for goal in &page.goals {
            for task in &goal.workflow.tasks {
                for execution in task.executions.iter().rev().take(2) {
                    let view = if let Some(binding) = &execution.thread {
                        let target = bitrouter_orchestrator::thread::ThreadTarget {
                            thread_id: binding.thread_id.clone(),
                            server_instance_id: self.threads.capabilities().server_instance_id,
                        };
                        match self
                            .threads
                            .read_stored_thread_view(&target, &CallerContext::local())
                            .await
                        {
                            Ok(view) => serde_json::json!({"view":view}),
                            Err(error) => serde_json::json!({"error":error.to_string()}),
                        }
                    } else {
                        serde_json::json!({"detail":"No linked Thread/Turn"})
                    };
                    executions.push(serde_json::json!({"goal_id":goal.id,"task_id":task.spec.id,
                        "execution_id":execution.id,"binding":execution.thread,"observation":view}));
                }
            }
        }
        Ok(
            serde_json::json!({"captured_at_ms":now()?,"goals":page,"executions":executions,
            "scope":"Bounded public Goal/Thread snapshots before fixture shutdown; separate reads, not an atomic filesystem/process/full-history snapshot."}),
        )
    }

    pub async fn finish(self, result: Result<()>) -> Result<()> {
        let keep = result.is_err() && std::env::var_os("BRO_GOAL_FIXTURE_KEEP_FAILURES").is_some();
        let file_database = !self
            .setup
            .config_text
            .contains("database: {url: 'sqlite::memory:'}");
        if keep {
            let root = self.setup.temporary.path();
            if let Some(error) = result.as_ref().err() {
                let _ = tokio::fs::write(root.join("failure.txt"), format!("{error:#}\n")).await;
            }
            let observed =
                tokio::time::timeout(Duration::from_secs(3), self.failure_runtime()).await;
            let value = match observed {
                Ok(Ok(value)) => value,
                other => serde_json::json!({"unavailable":format!("{other:?}")}),
            };
            if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
                let _ = tokio::fs::write(root.join("failure-runtime.json"), bytes).await;
            }
            let capture = if file_database {
                copy_database(&self.db, &root.join("failure-before-shutdown.sqlite")).await
            } else {
                Err(anyhow::anyhow!(
                    "in-memory database: public snapshots retained; no full database copy claimed"
                ))
            };
            let _ = tokio::fs::write(
                root.join("failure-before-copy.txt"),
                format!("{capture:?}\n"),
            )
            .await;
        }
        self.runtime.shutdown().await;
        self.threads.shutdown().await;
        self.supervisor.shutdown().await;
        self.ipc.abort();
        let _ = self.ipc.await;
        if keep {
            let root = self.setup.temporary.path();
            let capture = if file_database {
                copy_database(&self.db, &root.join("failure-after-shutdown.sqlite")).await
            } else {
                Err(anyhow::anyhow!(
                    "in-memory database: no full database copy claimed"
                ))
            };
            let _ = tokio::fs::write(
                root.join("failure-after-copy.txt"),
                format!("{capture:?}\n"),
            )
            .await;
            eprintln!(
                "Retained failed Goal fixture: {}",
                self.setup.temporary.keep().display()
            );
        }
        result
    }
}

/// SQLite produces a consistent independent copy; this is not an atomic
/// snapshot of filesystem, harness process or the supervisor's separate store.
pub(super) async fn copy_database(db: &sea_orm::DatabaseConnection, path: &Path) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(3),
        crate::upgrade_preflight::snapshot(db, path),
    )
    .await??;
    Ok(())
}
