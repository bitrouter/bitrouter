//! Built-in workers use ThreadService; Goal records only reference its facts.
use super::*;
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::goal::workflow::ThreadExecution;
use bitrouter_orchestrator::thread::{GoalWork, PermissionProfile, ThreadRequest, ThreadTarget};
use bitrouter_orchestrator::turn::{CancelTurnRequest, TurnRequest};

pub(super) fn source(goal: &GoalSnapshot, task: &Task, execution: &Execution) -> Result<GoalWork> {
    Ok(GoalWork {
        owner_thread_id: goal
            .workflow
            .owner_thread_id
            .clone()
            .context("Goal owner Thread is missing")?,
        goal_id: goal.id.clone(),
        task_id: task.spec.id.clone(),
        execution_id: execution.id.clone(),
        task_revision: execution.task_revision,
        control_source_revision: execution.control_source_revision,
    })
}

pub(super) fn turn_key(execution_id: &str) -> String {
    format!("goal-work-{execution_id}-turn")
}

impl Coordinator {
    pub(super) fn native_workspace(
        &self,
        goal: &GoalSnapshot,
        execution: &Execution,
    ) -> Result<PathBuf> {
        let expected = self.workspaces.join(&goal.id).join(&execution.id);
        let workspace = execution.workspace.canonicalize()?;
        anyhow::ensure!(
            execution.workspace == expected
                && workspace
                    == self
                        .workspaces
                        .canonicalize()?
                        .join(&goal.id)
                        .join(&execution.id),
            "native worker workspace is outside its managed Goal attempt"
        );
        // Restore the host's narrow managed-directory registration before cold
        // receipt reads. Existing registrations retain withdrawn profiles;
        // this neither resumes a Turn nor grants once-only tool permission.
        Ok(self.threads.register_local_workspace(&workspace)?)
    }

    pub(super) async fn launch_native(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution_id: &str,
        prompt: String,
    ) -> Result<()> {
        let execution = goal
            .workflow
            .tasks
            .iter()
            .flat_map(|task| &task.executions)
            .find(|execution| execution.id == execution_id)
            .context("native work attempt is missing")?;
        anyhow::ensure!(
            task.execution_harness(execution).is_none(),
            "attempt requested an external ACP worker"
        );
        let source = source(goal, task, execution)?;
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
                let config = self.routing.snapshot_config();
                super::resources::verify_mcp_configuration(
                    execution,
                    &super::resources::servers(&config, &task.spec.resources)?,
                )?;
                let model = task
                    .spec
                    .resources
                    .model_route
                    .as_deref()
                    .or_else(|| crate::policy_lock::native_default_model(&config))
                    .context("no built-in reasoning route is configured")?;
                let mut agent = AgentConfig::fixed(model, None)
                    .with_model_mode(crate::policy_lock::native_model_mode(&config, model)?);
                agent.max_context_bytes =
                    self.threads.capabilities().limits.context_bytes_per_thread;
                let servers = task
                    .spec
                    .resources
                    .mcp_servers
                    .iter()
                    .map(|id| {
                        config
                            .mcp_servers
                            .get(id)
                            .cloned()
                            .context("selected native MCP server disappeared")
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.threads
                    .create_goal_worker_thread(
                        &ThreadTarget {
                            thread_id: source.owner_thread_id.clone(),
                            server_instance_id: epoch.clone(),
                        },
                        &source,
                        ThreadRequest {
                            caller: caller.clone(),
                            workspace,
                            config: agent,
                            permission_profile: PermissionProfile::Ask,
                            verification_command: None,
                            idempotency_key: key,
                        },
                        Some(servers),
                    )
                    .await?
            }
        };
        anyhow::ensure!(
            thread.goal_work.as_ref() == Some(&source),
            "native worker belongs to another work source"
        );
        let target = ThreadTarget {
            thread_id: thread.thread_id.clone(),
            server_instance_id: epoch,
        };
        let key = turn_key(execution_id);
        let turn = match self
            .threads
            .find_goal_worker_turn(&target, &caller, &key)
            .await?
        {
            Some(turn) => turn,
            None => {
                self.threads
                    .wait_goal_worker_turn(
                        &target,
                        &caller,
                        source.clone(),
                        TurnRequest {
                            prompt,
                            idempotency_key: key,
                        },
                        control,
                    )
                    .await?
            }
        };
        self.change(
            goal,
            Change::BindWorkerTurn {
                execution_id: execution_id.into(),
                thread: ThreadExecution {
                    thread_id: turn.thread_id,
                    turn_id: turn.turn_id,
                },
            },
        )
        .await?;
        Ok(())
    }

    pub(super) async fn drive_worker(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<()> {
        if matches!(execution.observation, Observation::NotAdmitted { .. }) {
            return Ok(());
        }
        let caller = CallerContext::local();
        let epoch = self.threads.capabilities().server_instance_id;
        if execution.dispatch_requested {
            self.native_workspace(goal, execution)?;
        }
        let Some(binding) = &execution.thread else {
            if !execution.dispatch_requested {
                return self
                    .change(
                        goal,
                        Change::ObserveExecution {
                            execution_id: execution.id.clone(),
                            observation: Observation::Failed {
                                run_id: None,
                                reason:
                                    "Native workspace preparation ended before dispatch admission"
                                        .into(),
                            },
                        },
                    )
                    .await;
            }
            // A saved dispatch intent is not permission to repeat an unknown
            // admission. Find both actions by their original independent keys.
            let source = source(goal, task, execution)?;
            let key = bitrouter_orchestrator::service::worker::thread_key(&source)?;
            if let Some(thread) = self
                .threads
                .find_thread_admission(&epoch, &caller, &key)
                .await?
            {
                anyhow::ensure!(
                    thread.goal_work.as_ref() == Some(&source),
                    "recovered native Thread has a different source"
                );
                let target = ThreadTarget {
                    thread_id: thread.thread_id,
                    server_instance_id: epoch.clone(),
                };
                if let Some(turn) = self
                    .threads
                    .find_goal_worker_turn(&target, &caller, &turn_key(&execution.id))
                    .await?
                {
                    self.change(
                        goal,
                        Change::BindWorkerTurn {
                            execution_id: execution.id.clone(),
                            thread: ThreadExecution {
                                thread_id: turn.thread_id,
                                turn_id: turn.turn_id,
                            },
                        },
                    )
                    .await?;
                    return Ok(());
                }
            }
            if execution.managed_dispatch
                && let Observation::Unknown {
                    run_id: None,
                    reason,
                } = &execution.observation
            {
                // Unknown has already revoked this exact source. The shared
                // admission lock orders this lookup after any earlier accept
                // and before every future authorization check. An existing
                // Turn must be linked, even when its outcome is unresolved.
                if let Some(turn) = self
                    .threads
                    .find_goal_worker_admission(&epoch, &caller, &source, &turn_key(&execution.id))
                    .await?
                {
                    self.change(
                        goal,
                        Change::BindWorkerTurn {
                            execution_id: execution.id.clone(),
                            thread: ThreadExecution {
                                thread_id: turn.thread_id,
                                turn_id: turn.turn_id,
                            },
                        },
                    )
                    .await?;
                } else {
                    self.change(goal, Change::RejectWorkerDispatch {
                        execution_id:execution.id.clone(),
                        reason: format!("Original managed Turn was not admitted after source revocation; previous dispatch error: {reason}"),
                    }).await?;
                }
                return Ok(());
            }
            if bitrouter_orchestrator::goal::workflow::worker_requested(goal, &source) {
                // The original durable admission key is absent. Re-submit that
                // same action with its saved input; admission serializes a
                // concurrent acceptance rather than creating another attempt.
                let result = async {
                    let prepared = execution
                        .resources
                        .as_ref()
                        .context("native dispatch lacks its saved input")?;
                    let prompt = String::from_utf8(
                        crate::goal_artifacts::read(
                            &crate::goal_artifacts::root(&self.workspaces, &goal.id),
                            &prepared.prompt,
                        )
                        .await?,
                    )?;
                    match task.execution_harness(execution) {
                        Some(harness) => {
                            Box::pin(self.launch_acp(
                                goal,
                                task,
                                &execution.id,
                                prompt,
                                harness,
                                super::resources::servers(
                                    &self.routing.snapshot_config(),
                                    &prepared.selection,
                                )?,
                            ))
                            .await
                        }
                        None => {
                            Box::pin(self.launch_native(goal, task, &execution.id, prompt)).await
                        }
                    }
                }
                .await;
                return match result {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        self.change(
                            goal,
                            Change::ObserveExecution {
                                execution_id: execution.id.clone(),
                                observation: Observation::Unknown {
                                    run_id: None,
                                    reason: bounded_error(&error),
                                },
                            },
                        )
                        .await
                    }
                };
            }
            if matches!(execution.observation, Observation::Requested) {
                self.change(goal, Change::ObserveExecution { execution_id: execution.id.clone(),
                    observation: Observation::Unknown { run_id: None, reason: "Native dispatch has no confirmed original Thread/Turn admission; no automatic replay".into() } }).await?;
            }
            return Ok(());
        };
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: epoch,
        };
        let turn = self
            .threads
            .read_stored_turn(&target, &caller, &binding.turn_id)
            .await?;
        let external = task.execution_harness(execution).is_some();
        if let Some(session) = &turn.acp {
            anyhow::ensure!(
                task.execution_harness(execution) == Some(session.agent_id.as_str()),
                "worker session belongs to another harness"
            );
            if execution.confirmed_model_route.is_none()
                && let Some(route) = execution
                    .resources
                    .as_ref()
                    .and_then(|prepared| prepared.selection.model_route.as_ref())
                && session.confirmed_model_route.as_ref() == Some(route)
                && match &execution.observation {
                    Observation::Running { run_id, .. }
                    | Observation::Candidate { run_id, .. }
                    | Observation::Failed { run_id, .. }
                    | Observation::Unknown { run_id, .. } => {
                        run_id.as_ref() == Some(&session.run_id)
                    }
                    _ => false,
                }
            {
                return self
                    .change(
                        goal,
                        Change::ConfirmModelRoute {
                            execution_id: execution.id.clone(),
                            run_id: session.run_id.clone(),
                            route: route.clone(),
                        },
                    )
                    .await;
            }
        }
        let run_id = turn
            .acp
            .as_ref()
            .map(|session| session.run_id.clone())
            .or_else(|| turn.acp_stop.as_ref().and_then(|stop| stop.run_id.clone()));
        let session_id = turn.acp.as_ref().map(|session| session.session_id.clone());
        let terminal = matches!(
            turn.status,
            TurnStatus::Completed
                | TurnStatus::Failed
                | TurnStatus::Cancelled
                | TurnStatus::Interrupted
        );
        let stop_needed = goal.run_intent != RunIntent::Run
            || task.run_intent != RunIntent::Run
            || execution.task_revision != task.requirements_revision
            || goal.budget.admission(goal.created_at_ms, now()?).is_err()
            || matches!(
                execution.observation,
                Observation::Candidate { .. }
                    | Observation::Failed { .. }
                    | Observation::Unknown { .. }
            )
            || matches!(
                execution.stop,
                Some(StopState::Requested { .. } | StopState::Unknown { .. })
            );
        if stop_needed && !matches!(execution.stop, Some(StopState::Confirmed { .. })) {
            let _stop_gate = self.stop_gate.lock().await;
            let GoalReply::Snapshot { goal: current } = self
                .goals
                .execute(
                    "local",
                    GoalCommand::Get {
                        goal_id: goal.id.clone(),
                    },
                    now()?,
                )
                .await?
            else {
                anyhow::bail!("native stop snapshot unavailable");
            };
            let current_execution = current
                .workflow
                .tasks
                .iter()
                .flat_map(|task| &task.executions)
                .find(|known| known.id == execution.id)
                .context("native stop source disappeared")?;
            let operation_id = match &current_execution.stop {
                None => {
                    let operation_id = format!(
                        "{}-stop-{}",
                        if external { "acp" } else { "native" },
                        execution.id
                    );
                    self.change(
                        &current,
                        Change::RequestStop {
                            execution_id: execution.id.clone(),
                            operation_id: operation_id.clone(),
                        },
                    )
                    .await?;
                    operation_id
                }
                Some(
                    StopState::Requested { operation_id } | StopState::Unknown { operation_id, .. },
                ) => operation_id.clone(),
                Some(_) => return Ok(()),
            };
            let was_unknown = matches!(current_execution.stop, Some(StopState::Unknown { .. }));
            let stopped = if external {
                turn.acp_stop.as_ref().is_some_and(|stop| stop.confirmed)
            } else {
                terminal && !turn.unknown_effect
            };
            if (terminal || turn.status == TurnStatus::RecoveryRequired)
                && (!was_unknown || stopped)
            {
                self.change(
                    &current,
                    Change::ObserveStop {
                        execution_id: execution.id.clone(),
                        operation_id,
                        confirmed: stopped,
                        detail: format!(
                            "Native Turn {:?}; unknown_effect={}",
                            turn.status, turn.unknown_effect
                        ),
                    },
                )
                .await?;
            } else if !was_unknown
                && matches!(
                    turn.status,
                    TurnStatus::Accepted | TurnStatus::Running | TurnStatus::WaitingForInput
                )
            {
                self.threads
                    .cancel_turn(
                        &target,
                        &caller,
                        CancelTurnRequest {
                            turn_id: binding.turn_id.clone(),
                            idempotency_key: operation_id,
                        },
                    )
                    .await?;
            }
            return Ok(());
        }
        if matches!(
            execution.observation,
            Observation::Requested | Observation::Running { .. } | Observation::Unknown { .. }
        ) {
            let observation = match turn.status {
                TurnStatus::Running | TurnStatus::WaitingForInput
                    if !external || run_id.is_some() =>
                {
                    Some(Observation::Running {
                        run_id: run_id.clone(),
                        native_session_id: session_id.clone(),
                    })
                }
                TurnStatus::Completed if !turn.unknown_effect => {
                    let text = turn
                        .final_answer
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                        .context("completed native Turn has no candidate answer")?;
                    let report = crate::goal_artifacts::put(
                        &crate::goal_artifacts::root(&self.workspaces, &goal.id),
                        text.as_bytes(),
                    )
                    .await?;
                    Some(Observation::Candidate {
                        run_id: run_id.clone(),
                        native_session_id: session_id.clone(),
                        report,
                    })
                }
                TurnStatus::Failed | TurnStatus::Cancelled | TurnStatus::Interrupted
                    if !turn.unknown_effect =>
                {
                    Some(Observation::Failed {
                        run_id: run_id.clone(),
                        reason: turn
                            .detail
                            .unwrap_or_else(|| format!("Native Turn {:?}", turn.status)),
                    })
                }
                TurnStatus::RecoveryRequired
                | TurnStatus::Completed
                | TurnStatus::Failed
                | TurnStatus::Cancelled
                | TurnStatus::Interrupted => Some(Observation::Unknown {
                    run_id: run_id.clone(),
                    reason: "Shared worker execution requires effect recovery; no automatic replay"
                        .into(),
                }),
                _ => None,
            };
            if let Some(observation) = observation
                && observation != execution.observation
            {
                self.change(
                    goal,
                    Change::ObserveExecution {
                        execution_id: execution.id.clone(),
                        observation,
                    },
                )
                .await?;
            }
        } else if let Observation::Candidate { report, .. } = &execution.observation
            && matches!(execution.stop, Some(StopState::Confirmed { .. }))
            && goal.run_intent == RunIntent::Run
            && task.run_intent == RunIntent::Run
            && !execution
                .handoff
                .as_ref()
                .is_some_and(|handoff| handoff.task_revision == task.requirements_revision)
        {
            self.review(goal, task, execution, report).await?;
        }
        Ok(())
    }
}
