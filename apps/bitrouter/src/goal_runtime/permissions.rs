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
                || execution.stop_boundary_deadline_ms.is_some()
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
                    if matches!(execution.stop, Some(StopState::Confirmed { .. })) {
                        let response = self
                            .supervisor
                            .dispatch(SessionRequest::authorized(
                                &self.grant,
                                SessionCommand::Snapshot {
                                    run_id: decision.run_id.clone(),
                                },
                            ))
                            .await?;
                        let SessionResponse::Snapshot { snapshot } = response else {
                            anyhow::bail!("stopped permission snapshot is unavailable");
                        };
                        if snapshot.run_id == decision.run_id
                            && snapshot.process == ProcessState::Stopped
                            && !snapshot
                                .pending_permissions
                                .iter()
                                .any(|pending| pending.permission_id == decision.permission_id)
                        {
                            self.change(&current, Change::Decision { event: Event::Obsolete {
                                decision_id: decision.id.clone(),
                                reason: "The original owned session is stopped and this request is no longer pending. No user consent or tool completion follows from closing it.".into(),
                            }}).await?;
                            continue;
                        }
                    }
                    if current.run_intent != RunIntent::Run
                        || task.run_intent != RunIntent::Run
                        || execution.stop.is_some()
                        || execution.stop_boundary_deadline_ms.is_some()
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
        let material = match self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Events {
                    run_id: run_id.into(),
                    after_seq: 0,
                },
            ))
            .await
        {
            Ok(SessionResponse::Events { replay }) => {
                crate::goal_permission_material::capture(permission, run_id, &replay)?
            }
            _ => crate::goal_permission_material::unavailable(
                permission,
                "Current tool history is unavailable",
            )?,
        };
        let request = crate::goal_artifacts::put(&root, &material).await?;
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
            self.validate_permission_delivery(goal, decision).await?;
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

    async fn validate_permission_delivery(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
    ) -> Result<()> {
        let response = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: decision.run_id.clone(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot } = response else {
            anyhow::bail!("unexpected current permission snapshot");
        };
        let permission = snapshot
            .pending_permissions
            .iter()
            .find(|permission| permission.permission_id == decision.permission_id)
            .context("permission is no longer pending on its original run")?;
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
            anyhow::bail!("unexpected current permission events");
        };
        let current =
            crate::goal_permission_material::capture(permission, &decision.run_id, &replay)?;
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let saved = crate::goal_artifacts::read(&root, &decision.request).await?;
        crate::goal_permission_material::validate_current(&saved, &current, &decision.run_id)
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
        Ok(choice_was_selected(
            &replay.events,
            &decision.permission_id,
            option_id,
        ))
    }
}

fn choice_was_selected(
    events: &[crate::supervisor::SessionEvent],
    permission: &str,
    option: &str,
) -> bool {
    use crate::supervisor::{PermissionResolutionOrigin, SessionEventKind};
    let mut resolutions = events.iter().filter(|event| {
        matches!(&event.kind,
        SessionEventKind::PermissionResolved { permission_id, .. } if permission_id == permission)
    });
    let Some(resolution) = resolutions.next() else {
        return false;
    };
    if resolutions.next().is_some() {
        return false;
    }
    matches!(&resolution.kind, SessionEventKind::PermissionResolved {
        option_id: Some(selected), automatic: false,
        origin: Some(PermissionResolutionOrigin::ControllerChoice), ..
    } | SessionEventKind::PermissionResolved {
        option_id: Some(selected), automatic: true,
        origin: Some(PermissionResolutionOrigin::ConfiguredPolicy), ..
    } if selected == option)
}

pub(super) async fn allowed_local_option(
    root: &Path,
    decision: &PermissionDecision,
    workspace: &Path,
) -> Result<Option<String>> {
    let bytes = crate::goal_artifacts::read(root, &decision.request).await?;
    let Some(tool) = crate::goal_permission_material::effective_tool(&bytes, &decision.run_id)?
    else {
        return Ok(None);
    };
    if !is_local_tool(&tool, workspace).await? {
        return Ok(None);
    }
    Ok(decision
        .options
        .iter()
        .find(|option| option.allows && !option.persistent)
        .map(|option| option.id.clone()))
}

pub(super) async fn is_local_tool(tool: &serde_json::Value, workspace: &Path) -> Result<bool> {
    if !matches!(
        tool.get("kind").and_then(serde_json::Value::as_str),
        Some("read" | "edit" | "search")
    ) {
        return Ok(false);
    }
    // ACP locations are optional display metadata. Explicit path arguments
    // remain separate original declarations; check every supplied source.
    let mut paths = Vec::new();
    if let Some(value) = tool.get("locations").filter(|value| !value.is_null()) {
        let Some(locations) = value.as_array() else {
            return Ok(false);
        };
        if locations.len() > 32 {
            return Ok(false);
        }
        for location in locations {
            let Some(path) = location.get("path").and_then(serde_json::Value::as_str) else {
                return Ok(false);
            };
            paths.push(path);
        }
    }
    if let Some(value) = tool.get("rawInput").filter(|value| !value.is_null()) {
        let Some(arguments) = value.as_object() else {
            return Ok(false);
        };
        for key in ["path", "file_path", "filePath"] {
            if let Some(value) = arguments.get(key).filter(|value| !value.is_null()) {
                let Some(path) = value.as_str() else {
                    return Ok(false);
                };
                paths.push(path);
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    if paths.is_empty() || paths.len() > 32 {
        return Ok(false);
    }
    let workspace = tokio::fs::canonicalize(workspace).await?;
    for path in paths {
        if path.is_empty() || path.contains("://") {
            return Ok(false);
        }
        let path = Path::new(path);
        if tool.get("kind").and_then(serde_json::Value::as_str) == Some("edit")
            && path.components().any(|part| {
                matches!(part,
                std::path::Component::Normal(name) if name == ".git" || name == ".bro")
            })
        {
            return Ok(false);
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
            return Ok(false);
        }
        while !tokio::fs::try_exists(&target).await? {
            let Some(parent) = target.parent() else {
                return Ok(false);
            };
            target = parent.to_path_buf();
        }
        if !tokio::fs::canonicalize(target)
            .await?
            .starts_with(&workspace)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
