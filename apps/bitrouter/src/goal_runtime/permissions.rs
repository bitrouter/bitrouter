use super::*;
use crate::supervisor::{PendingPermissionSnapshot, SessionAction, SessionMutation};
use bitrouter_orchestrator::goal::decision::{
    Event, PermissionDecision, PermissionOption, PermissionState,
};

impl Coordinator {
    pub(super) async fn drive_permissions(&self, goal: &GoalSnapshot) -> Result<()> {
        for task in &goal.workflow.tasks {
            let Some(execution) = task.executions.last() else {
                continue;
            };
            let Observation::Running { run_id, .. } = &execution.observation else {
                continue;
            };
            if goal.run_intent != RunIntent::Run
                || task.run_intent != RunIntent::Run
                || execution.task_revision != task.requirements_revision
                || execution.stop.is_some()
            {
                continue;
            }
            let response = self
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &self.grant,
                    SessionCommand::Snapshot {
                        run_id: run_id.clone(),
                    },
                ))
                .await?;
            let SessionResponse::Snapshot { snapshot } = response else {
                anyhow::bail!("unexpected permission snapshot");
            };
            for permission in &snapshot.pending_permissions {
                self.observe_permission(goal, task, execution, run_id, permission)
                    .await?;
            }
        }
        let response = self
            .goals
            .execute(
                "local",
                GoalCommand::Get {
                    goal_id: goal.id.clone(),
                },
                now()?,
            )
            .await?;
        let GoalReply::Snapshot { goal: current } = response else {
            anyhow::bail!("unexpected permission Goal snapshot");
        };
        for decision in &current.workflow.decisions {
            match &decision.state {
                PermissionState::Pending => {
                    let task = current.workflow.tasks.iter().find(|task| {
                        task.executions
                            .iter()
                            .any(|execution| execution.id == decision.execution_id)
                    });
                    let applicable = task
                        .is_some_and(|task| task.requirements_revision == decision.task_revision)
                        && current.requirements_revision == decision.requirements_revision;
                    if !applicable {
                        self.change(
                            &current,
                            Change::Decision {
                                event: Event::Obsolete {
                                    decision_id: decision.id.clone(),
                                    reason:
                                        "Permission belongs to superseded requirements or execution"
                                            .into(),
                                },
                            },
                        )
                        .await?;
                        continue;
                    }
                    let Some(task) = task else {
                        continue;
                    };
                    let Some(execution) = task
                        .executions
                        .iter()
                        .find(|execution| execution.id == decision.execution_id)
                    else {
                        continue;
                    };
                    if current.run_intent != RunIntent::Run
                        || task.run_intent != RunIntent::Run
                        || execution.stop.is_some()
                    {
                        continue;
                    }
                    let root = crate::goal_artifacts::root(&self.workspaces, &current.id);
                    if let Some(option_id) =
                        allowed_local_option(&root, decision, &execution.workspace).await?
                    {
                        let operation_id = uuid::Uuid::new_v4().to_string();
                        self.change(
                            &current,
                            Change::Decision {
                                event: Event::Respond {
                                    decision_id: decision.id.clone(),
                                    operation_id: operation_id.clone(),
                                    option_id: option_id.clone(),
                                    automatic: true,
                                },
                            },
                        )
                        .await?;
                        self.send_permission(&current, decision, &operation_id, &option_id)
                            .await?;
                    }
                }
                PermissionState::Requested {
                    operation_id,
                    option_id,
                    dispatch_requested,
                    ..
                } => {
                    if !dispatch_requested {
                        self.send_permission(&current, decision, operation_id, option_id)
                            .await?;
                        continue;
                    }
                    // This job did not admit that response. Recover from actual
                    // resolution events instead of blindly resending approval.
                    let submitted = self.permission_was_selected(decision, option_id).await?;
                    self.change(&current, Change::Decision { event: Event::Result { decision_id: decision.id.clone(), operation_id: operation_id.clone(), submitted,
                        detail: if submitted { "Controller event confirms option selection; wire transmission and tool effects remain unconfirmed".into() } else { "Response dispatch survived without a confirmed controller selection; no automatic replay".into() } } }).await?;
                }
                PermissionState::Unknown {
                    operation_id,
                    option_id,
                    ..
                } if self.permission_was_selected(decision, option_id).await? => {
                    self.change(&current, Change::Decision { event: Event::Result {
                            decision_id: decision.id.clone(), operation_id: operation_id.clone(), submitted: true,
                            detail: "Late controller selection evidence reconciled the response; tool effects remain unconfirmed".into(),
                    } }).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn observe_permission(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
        run_id: &str,
        permission: &PendingPermissionSnapshot,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        let id = hex::encode(Sha256::digest(serde_json::to_vec(&(
            run_id,
            &permission.permission_id,
            goal.requirements_revision,
            task.requirements_revision,
        ))?));
        if goal
            .workflow
            .decisions
            .iter()
            .any(|decision| decision.id == id)
        {
            return Ok(());
        }
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let request = crate::goal_artifacts::put(&root, &serde_json::to_vec(permission)?).await?;
        let options = permission
            .options
            .iter()
            .map(|option| {
                let kind = serde_json::to_value(option.kind)?;
                Ok(PermissionOption {
                    id: option.option_id.0.to_string(),
                    label: option.name.to_string(),
                    allows: matches!(kind.as_str(), Some("allow_once" | "allow_always")),
                    persistent: matches!(kind.as_str(), Some("allow_always" | "reject_always")),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.change(
            goal,
            Change::Decision {
                event: Event::Observe {
                    decision: PermissionDecision {
                        id,
                        execution_id: execution.id.clone(),
                        run_id: run_id.into(),
                        permission_id: permission.permission_id.clone(),
                        requirements_revision: goal.requirements_revision,
                        task_revision: task.requirements_revision,
                        request,
                        options,
                        state: PermissionState::Pending,
                    },
                },
            },
        )
        .await
    }

    async fn send_permission(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
        operation_id: &str,
        option_id: &str,
    ) -> Result<()> {
        let _control = self.stop_gate.lock().await;
        let result = async {
            self.change(
                goal,
                Change::Decision {
                    event: Event::RequestDelivery {
                        decision_id: decision.id.clone(),
                        operation_id: operation_id.into(),
                    },
                },
            )
            .await?;
            let response = self
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &self.grant,
                    SessionCommand::AcquireLease {
                        run_id: decision.run_id.clone(),
                        client_id: self.grant.client_id.clone(),
                        mode: LeaseMode::Transient,
                        action_request_id: format!("{operation_id}-lease"),
                        takeover: false,
                    },
                ))
                .await?;
            let SessionResponse::Lease { lease } = response else {
                anyhow::bail!("permission response has no control lease");
            };
            let response = self
                .supervisor
                .dispatch(SessionRequest::authorized(
                    &self.grant,
                    SessionCommand::Mutate(SessionMutation {
                        fence: ControlFence {
                            run_id: decision.run_id.clone(),
                            client_id: self.grant.client_id.clone(),
                            lease_generation: lease.generation,
                            action_request_id: operation_id.into(),
                        },
                        action: SessionAction::Permission {
                            permission_id: decision.permission_id.clone(),
                            option_id: option_id.into(),
                        },
                    }),
                ))
                .await?;
            anyhow::ensure!(
                matches!(response, SessionResponse::Action { .. }),
                "permission response returned no mutation acknowledgement"
            );
            self.permission_was_selected(decision, option_id).await
        }
        .await;
        let (submitted, detail) = match result {
            Ok(true) => (
                true,
                "Controller confirms permission option selection; wire transmission and tool effects remain unconfirmed".into(),
            ),
            Ok(false) => (
                false,
                "Permission mutation has no matching resolution evidence".into(),
            ),
            Err(error) => (false, bounded_error(&error)),
        };
        self.change(
            goal,
            Change::Decision {
                event: Event::Result {
                    decision_id: decision.id.clone(),
                    operation_id: operation_id.into(),
                    submitted,
                    detail,
                },
            },
        )
        .await
    }

    async fn permission_was_selected(
        &self,
        decision: &PermissionDecision,
        option_id: &str,
    ) -> Result<bool> {
        let response = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Events {
                    run_id: decision.run_id.clone(),
                    after_seq: 0,
                },
            ))
            .await?;
        let SessionResponse::Events { replay } = response else {
            anyhow::bail!("unexpected permission events response");
        };
        Ok(replay.events.iter().any(|event| matches!(&event.kind, crate::supervisor::SessionEventKind::PermissionResolved { permission_id, option_id: Some(selected), .. } if permission_id == &decision.permission_id && selected == option_id)))
    }
}

pub(super) async fn allowed_local_option(
    root: &Path,
    decision: &PermissionDecision,
    workspace: &Path,
) -> Result<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_slice(&crate::goal_artifacts::read(root, &decision.request).await?)?;
    let Some(tool) = value.get("tool_call") else {
        return Ok(None);
    };
    if !matches!(
        tool.get("kind").and_then(serde_json::Value::as_str),
        Some("read" | "edit" | "search")
    ) {
        return Ok(None);
    }
    let Some(locations) = tool.get("locations").and_then(serde_json::Value::as_array) else {
        return Ok(None);
    };
    if locations.is_empty() || locations.len() > 32 {
        return Ok(None);
    }
    let workspace = tokio::fs::canonicalize(workspace).await?;
    for location in locations {
        let Some(path) = location.get("path").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let path = Path::new(path);
        if tool.get("kind").and_then(serde_json::Value::as_str) == Some("edit")
            && path.components().any(|part| {
                matches!(part,
                std::path::Component::Normal(name) if name == ".git" || name == ".bro")
            })
        {
            return Ok(None);
        }
        let mut target = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        if path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Ok(None);
        }
        while !tokio::fs::try_exists(&target).await? {
            let Some(parent) = target.parent() else {
                return Ok(None);
            };
            target = parent.to_path_buf();
        }
        if !tokio::fs::canonicalize(target)
            .await?
            .starts_with(&workspace)
        {
            return Ok(None);
        }
    }
    Ok(decision
        .options
        .iter()
        .find(|option| option.allows && !option.persistent)
        .map(|option| option.id.clone()))
}

#[cfg(test)]
mod tests;
