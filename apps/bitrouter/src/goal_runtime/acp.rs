//! External Goal workers are adapters of the same Thread/Turn service as native work.
use super::*;
use bitrouter_orchestrator::control::WorkControl;
use bitrouter_orchestrator::service::worker::acp::{Binding, Driver, Outcome, Owner, Stop};
use bitrouter_orchestrator::thread::executor::{AcpConfig, AcpThreadRequest};
use bitrouter_orchestrator::thread::{PermissionProfile, ThreadTarget};
use bitrouter_orchestrator::turn::TurnRequest;

struct Adapter {
    supervisor: Supervisor,
    threads: bitrouter_orchestrator::service::ThreadService,
    request: StartRunRequest,
    control: Arc<dyn WorkControl>,
}

struct Dispatch<'a> {
    adapter: &'a Adapter,
    owner: &'a Owner,
    key: &'a str,
}

#[async_trait::async_trait]
impl crate::supervisor::managed::DispatchGuard for Dispatch<'_> {
    async fn authorize(&self) -> Result<()> {
        self.adapter
            .threads
            .authorize_acp_dispatch(self.owner, self.key, self.adapter.control.as_ref())
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Driver for Adapter {
    async fn start(&self, owner: &Owner, key: &str) -> std::result::Result<Binding, String> {
        let mut request = self.request.clone();
        request.action_request_id = key.into();
        let response = self
            .supervisor
            .start_managed(
                request,
                owner.clone(),
                &Dispatch {
                    adapter: self,
                    owner,
                    key,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))?;
        let SessionResponse::Started { snapshot, .. } = response else {
            return Err("ACP startup returned no owned session".into());
        };
        if snapshot.process != ProcessState::Running {
            return Err("Original ACP process is not confirmed running; no startup replay".into());
        }
        Ok(Binding {
            run_id: snapshot.run_id,
            session_id: snapshot
                .native_session_id
                .ok_or("ACP session ID was not reported")?,
            agent_id: snapshot.agent_id,
            confirmed_model_route: snapshot.confirmed_route,
            capabilities: snapshot
                .capabilities
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| error.to_string())?,
        })
    }
    async fn prompt(
        &self,
        owner: &Owner,
        binding: &Binding,
        key: &str,
        text: &str,
    ) -> std::result::Result<(), String> {
        self.supervisor
            .managed_prompt(
                owner,
                &binding.run_id,
                key,
                text,
                &Dispatch {
                    adapter: self,
                    owner,
                    key,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))
    }
    async fn permission(
        &self,
        owner: &Owner,
        binding: &Binding,
        key: &str,
        request: &str,
        option: &str,
    ) -> std::result::Result<(), String> {
        self.supervisor
            .managed_permission(
                owner,
                &binding.run_id,
                key,
                request,
                option,
                &Dispatch {
                    adapter: self,
                    owner,
                    key,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))
    }
    async fn stop(
        &self,
        owner: &Owner,
        binding: Option<&Binding>,
        key: &str,
    ) -> std::result::Result<Stop, String> {
        self.supervisor
            .managed_stop(owner, binding.map(|binding| binding.run_id.as_str()), key)
            .await
            .map_err(|error| format!("{error:#}"))
    }
    async fn observe(
        &self,
        owner: &Owner,
        binding: &Binding,
        after: u64,
    ) -> std::result::Result<bitrouter_orchestrator::service::worker::acp::Observation, String>
    {
        let (snapshot, replay) = self
            .supervisor
            .managed_observation(owner, &binding.run_id, after)
            .await
            .map_err(|error| format!("{error:#}"))?;
        let mut answer_delta = String::new();
        let mut outcome = None;
        let mut events = Vec::new();
        let mut cursor = after;
        let mut history_complete = replay.history_complete;
        for event in replay.events.iter().take(64) {
            cursor = event.seq;
            match &event.kind {
                crate::supervisor::SessionEventKind::PayloadOmitted { .. } => {
                    history_complete = false
                }
                crate::supervisor::SessionEventKind::Update { update } => {
                    if let Some(SessionUpdateKind::MessageChunk { text, .. }) =
                        translate(update.clone())
                    {
                        answer_delta.push_str(&text);
                    }
                }
                crate::supervisor::SessionEventKind::TurnSettled { result } => {
                    outcome = Some(if result.stop_reason == "end_turn" {
                        Outcome::Completed
                    } else {
                        Outcome::Unknown(format!("ACP prompt settled with {}", result.stop_reason))
                    });
                }
                crate::supervisor::SessionEventKind::TurnFailed { message } => {
                    outcome = Some(Outcome::Unknown(message.clone()))
                }
                _ => {}
            }
            events.push(serde_json::to_value(event).map_err(|error| error.to_string())?);
        }
        if outcome.is_none()
            && replay.events.is_empty()
            && matches!(
                snapshot.process,
                ProcessState::Failed | ProcessState::Interrupted
            )
        {
            outcome =
                Some(Outcome::Unknown(snapshot.failure.clone().unwrap_or_else(
                    || "ACP process continuity is unavailable".into(),
                )));
        }
        Ok(bitrouter_orchestrator::service::worker::acp::Observation {
            cursor,
            snapshot_cursor: snapshot.last_seq,
            history_complete,
            events,
            answer_delta,
            outcome,
            permissions: snapshot
                .pending_permissions
                .into_iter()
                .map(
                    |permission| bitrouter_orchestrator::service::worker::acp::Permission {
                        request_id: permission.permission_id,
                        tool_call: permission.tool_call,
                        options: permission.options,
                    },
                )
                .collect(),
        })
    }
}

impl Coordinator {
    pub(super) async fn acp_run_snapshot(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<crate::supervisor::RunSnapshot> {
        let Some(link) = &execution.thread else {
            return self
                .supervisor
                .start_snapshot(&self.grant, &execution.id)
                .await?
                .context("original execution has no recorded run mapping");
        };
        let target = ThreadTarget {
            thread_id: link.thread_id.clone(),
            server_instance_id: self.threads.capabilities().server_instance_id,
        };
        let view = self
            .threads
            .read_stored_thread_view(&target, &CallerContext::local())
            .await?;
        anyhow::ensure!(
            view.thread.goal_work.as_ref() == Some(&super::native::source(goal, task, execution)?),
            "ACP execution source does not match its Thread"
        );
        let turn = self
            .threads
            .read_stored_turn(&target, &CallerContext::local(), &link.turn_id)
            .await?;
        let run_id = turn
            .acp
            .as_ref()
            .map(|session| session.run_id.clone())
            .or_else(|| turn.acp_stop.as_ref().and_then(|stop| stop.run_id.clone()))
            .context("ACP admission has no actual process mapping")?;
        let SessionResponse::Snapshot { snapshot } = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot { run_id },
            ))
            .await?
        else {
            anyhow::bail!("ACP process observation unavailable");
        };
        anyhow::ensure!(
            snapshot.managed_by.as_ref().is_some_and(
                |owner| owner.thread_id == link.thread_id && owner.turn_id == link.turn_id
            ) && task.execution_harness(execution) == Some(snapshot.agent_id.as_str()),
            "ACP process belongs to another accepted Turn"
        );
        Ok(*snapshot)
    }

    pub(super) async fn launch_acp(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution_id: &str,
        prompt: String,
        harness: &str,
        mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    ) -> Result<()> {
        let execution = goal
            .workflow
            .tasks
            .iter()
            .flat_map(|task| &task.executions)
            .find(|execution| execution.id == execution_id)
            .context("ACP work attempt missing")?;
        anyhow::ensure!(
            task.execution_harness(execution) == Some(harness),
            "ACP startup differs from the attempt's requested worker"
        );
        let source = super::native::source(goal, task, execution)?;
        let control = model_control::ModelControl::for_worker(self, goal, source.clone());
        control.gate().await?;
        let workspace = self.native_workspace(goal, execution)?;
        let epoch = self.threads.capabilities().server_instance_id;
        let caller = CallerContext::local();
        let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
        let thread = match self
            .threads
            .find_thread_admission(&epoch, &caller, &key)
            .await?
        {
            Some(thread) => thread,
            None => {
                self.threads
                    .create_goal_acp_thread(
                        &ThreadTarget {
                            thread_id: source.owner_thread_id.clone(),
                            server_instance_id: epoch.clone(),
                        },
                        &source,
                        AcpThreadRequest {
                            caller: caller.clone(),
                            workspace: workspace.clone(),
                            config: AcpConfig {
                                agent_id: harness.into(),
                                requested_model_route: task.spec.resources.model_route.clone(),
                                max_duration: Duration::from_secs(600),
                            },
                            permission_profile: PermissionProfile::Ask,
                            idempotency_key: key,
                        },
                    )
                    .await?
            }
        };
        anyhow::ensure!(
            thread.goal_work.as_ref() == Some(&source),
            "ACP worker belongs to another Goal source"
        );
        let target = ThreadTarget {
            thread_id: thread.thread_id,
            server_instance_id: epoch,
        };
        let key = super::native::turn_key(execution_id);
        let turn = match self
            .threads
            .find_goal_worker_turn(&target, &caller, &key)
            .await?
        {
            Some(turn) => turn,
            None => {
                let driver = Arc::new(Adapter {
                    supervisor: self.supervisor.clone(),
                    threads: self.threads.clone(),
                    control: control.clone(),
                    request: StartRunRequest {
                        action_request_id: String::new(),
                        client_id: None,
                        label: Some(format!("{} / {}", goal.title, task.spec.title)),
                        agent_id: harness.into(),
                        prompt: None,
                        cwd: workspace,
                        routing: Default::default(),
                        launch: crate::acp_cli::LaunchOptions {
                            required_session_route: task.spec.resources.model_route.clone(),
                            mcp_servers,
                            ..Default::default()
                        },
                        session: Default::default(),
                        presentation: crate::supervisor::Presentation::Background,
                        parent_run_id: None,
                        allow_shared_directory: false,
                        permission_policy: Default::default(),
                        result_schema: None,
                    },
                });
                self.threads
                    .wait_goal_acp_turn(
                        &target,
                        &caller,
                        source,
                        TurnRequest {
                            prompt,
                            idempotency_key: key,
                        },
                        control,
                        driver,
                    )
                    .await?
            }
        };
        self.change(
            goal,
            Change::BindWorkerTurn {
                execution_id: execution_id.into(),
                thread: bitrouter_orchestrator::goal::workflow::ThreadExecution {
                    thread_id: turn.thread_id,
                    turn_id: turn.turn_id,
                },
            },
        )
        .await
    }
}
