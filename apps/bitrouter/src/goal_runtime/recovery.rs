//! Replacements require recorded, finished local tool work and a frozen workspace.
use super::*;
use crate::supervisor::{ReplayBatch, SessionEventKind};
use bitrouter_orchestrator::goal::decision::{Event as PermissionEvent, PermissionDecision};
use bitrouter_orchestrator::goal::workflow::LocalReconciliation;

impl Coordinator {
    /// Read committed observations/results at one cutoff; the live head prunes closed cards.
    async fn permission_receipts(
        &self,
        goal: &GoalSnapshot,
        execution: &Execution,
    ) -> Result<Vec<(PermissionDecision, Option<String>)>> {
        let mut after = 0;
        let mut requests = BTreeMap::<String, (PermissionDecision, Option<String>)>::new();
        loop {
            let response = self
                .goals
                .execute(
                    "local",
                    GoalCommand::History {
                        goal_id: goal.id.clone(),
                        after,
                        cutoff: Some(goal.revision),
                        limit: bitrouter_orchestrator::goal::MAX_PAGE_ITEMS,
                    },
                    now()?,
                )
                .await?;
            let GoalReply::History { page } = response else {
                anyhow::bail!("permission history is unavailable");
            };
            anyhow::ensure!(
                page.cutoff == goal.revision,
                "permission history cutoff changed"
            );
            for operation in page.operations {
                let bitrouter_orchestrator::goal::OperationKind::Workflow { change } =
                    operation.kind
                else {
                    continue;
                };
                match *change {
                    Change::Decision {
                        event: PermissionEvent::Observe { decision },
                    } if decision.execution_id == execution.id => {
                        requests.insert(decision.id.clone(), (decision, None));
                    }
                    Change::Decision {
                        event:
                            PermissionEvent::Result {
                                decision_id,
                                operation_id,
                                submitted,
                                ..
                            },
                    } => {
                        if let Some((_, delivered)) = requests.get_mut(&decision_id) {
                            *delivered = submitted.then_some(operation_id);
                        }
                    }
                    _ => {}
                }
            }
            let Some(next) = page.next_after else {
                break;
            };
            anyhow::ensure!(next > after, "permission history did not advance");
            after = next;
        }
        Ok(requests.into_values().collect())
    }

    pub(super) async fn reconcile_local_execution(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<()> {
        let control_source_revision = goal
            .control_source_revision
            .max(task.control_source_revision);
        if execution
            .reconciliation
            .as_ref()
            .is_some_and(|state| match state {
                LocalReconciliation::Verified {
                    task_revision,
                    control_source_revision: observed,
                    ..
                }
                | LocalReconciliation::Blocked {
                    task_revision,
                    control_source_revision: observed,
                    ..
                } => {
                    *task_revision == task.requirements_revision
                        && *observed == control_source_revision
                }
            })
        {
            return Ok(());
        }
        let result = self.local_recovery_evidence(goal, task, execution).await;
        let reconciliation = match result {
            Ok((workspace, transcript)) => LocalReconciliation::Verified {
                task_revision: task.requirements_revision,
                control_source_revision,
                workspace,
                transcript,
            },
            Err(error) => LocalReconciliation::Blocked {
                task_revision: task.requirements_revision,
                control_source_revision,
                reason: format!(
                    "No automatic replacement: {}. Reconcile the missing tool/workspace/external-effect evidence before retrying.",
                    bounded_error(&error)
                ),
            },
        };
        self.change(
            goal,
            Change::ReconcileStoppedExecution {
                execution_id: execution.id.clone(),
                reconciliation,
            },
        )
        .await
    }

    async fn local_recovery_evidence(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<(ArtifactRef, ArtifactRef)> {
        let run_id = match &execution.observation {
            Observation::Running { run_id, .. } | Observation::Candidate { run_id, .. } => run_id,
            Observation::Unknown {
                run_id: Some(run_id),
                ..
            }
            | Observation::Failed {
                run_id: Some(run_id),
                ..
            } => run_id,
            _ => anyhow::bail!("the original dispatch has no actual run mapping"),
        };
        anyhow::ensure!(
            matches!(execution.stop, Some(StopState::Confirmed { .. })),
            "owned process stop is unconfirmed"
        );
        anyhow::ensure!(
            !matches!(execution.review, Some(ReviewState::Requested { .. }))
                && !execution.checks.iter().any(|check| matches!(
                    check.state,
                    bitrouter_orchestrator::goal::workflow::CheckState::Requested
                        | bitrouter_orchestrator::goal::workflow::CheckState::Observed {
                            completed: false,
                            ..
                        }
                )),
            "BRO check or semantic review is still unresolved"
        );
        let snapshot = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: run_id.clone(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot } = snapshot else {
            anyhow::bail!("stop snapshot is unavailable");
        };
        anyhow::ensure!(
            snapshot.run_id == *run_id
                && snapshot.process == ProcessState::Stopped
                && snapshot.pending_permissions.is_empty(),
            "owned process is not stopped or has unresolved permissions"
        );
        anyhow::ensure!(
            tokio::fs::canonicalize(&snapshot.cwd).await?
                == tokio::fs::canonicalize(&execution.workspace).await?,
            "run workspace does not match the committed execution"
        );
        let replay = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Events {
                    run_id: run_id.clone(),
                    after_seq: 0,
                },
            ))
            .await?;
        let SessionResponse::Events { replay } = replay else {
            anyhow::bail!("original tool history is unavailable");
        };
        anyhow::ensure!(
            replay.history_complete
                && replay.first_retained_seq == 1
                && replay.snapshot_seq == snapshot.last_seq,
            "original tool history is incomplete or changed"
        );
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let mut approved = BTreeSet::new();
        let receipts = self.permission_receipts(goal, execution).await?;
        let mut submissions = Vec::new();
        for event in &replay.events {
            let SessionEventKind::Permission { permission } = &event.kind else {
                continue;
            };
            let actual_request = serde_json::to_vec(permission)?;
            let mut matching = None;
            for (decision, delivery) in &receipts {
                if decision.run_id == *run_id
                    && decision.permission_id == permission.permission_id
                    && let Some(delivery) = delivery
                    && crate::goal_artifacts::read(&root, &decision.request).await?
                        == actual_request
                {
                    matching = Some((decision, delivery));
                    break;
                }
            }
            let (decision, delivery) =
                matching.context("tool history has no matching durable permission submission")?;
            let option_id = replay
                .events
                .iter()
                .rev()
                .find_map(|event| match &event.kind {
                    SessionEventKind::PermissionResolved {
                        permission_id,
                        option_id,
                        ..
                    } if permission_id == &permission.permission_id => option_id.as_ref(),
                    _ => None,
                })
                .context("actual permission response is unresolved")?;
            let local =
                permissions::allowed_local_option(&root, decision, &execution.workspace).await?;
            anyhow::ensure!(
                local.as_ref() == Some(option_id),
                "permission included unsupported, persistent, external or unscoped effects"
            );
            let bytes = crate::goal_artifacts::read(&root, &decision.request).await?;
            let request: serde_json::Value = serde_json::from_slice(&bytes)?;
            approved.insert(
                request["tool_call"]["toolCallId"]
                    .as_str()
                    .context("permission omitted its actual tool identity")?
                    .to_owned(),
            );
            submissions.push(serde_json::json!({"decision_id":decision.id,"operation_id":delivery,
                "permission_id":permission.permission_id,"actual_option_id":option_id,"request_sha256":decision.request.sha256}));
        }
        completed_local_tools(&replay, &approved)?;
        let base = execution
            .base_commit
            .as_deref()
            .context("workspace has no committed baseline")?;
        let workspace = crate::goal_artifacts::capture(&root, &execution.workspace, base).await?;
        let repeated = crate::goal_artifacts::capture(&root, &execution.workspace, base).await?;
        anyhow::ensure!(workspace == repeated, "stopped workspace is still changing");
        let again = self
            .supervisor
            .dispatch(SessionRequest::authorized(
                &self.grant,
                SessionCommand::Snapshot {
                    run_id: run_id.clone(),
                },
            ))
            .await?;
        let SessionResponse::Snapshot { snapshot: again } = again else {
            anyhow::bail!("final stop snapshot is unavailable");
        };
        anyhow::ensure!(
            again.process == ProcessState::Stopped
                && again.last_seq == replay.snapshot_seq
                && again.pending_permissions.is_empty(),
            "process or tool state changed during reconciliation"
        );
        let evidence = serde_json::json!({"scope":"finished declared read/edit/search inside the owned workspace only",
            "execution_id":execution.id,"run_id":run_id,"task_revision":task.requirements_revision,
            "stop":execution.stop,"snapshot":again,"approved_local_tools":approved,"permission_submissions":submissions,"workspace":workspace,"history":replay});
        let transcript = crate::goal_artifacts::put(&root, &serde_json::to_vec(&evidence)?).await?;
        Ok((workspace, transcript))
    }
}

fn completed_local_tools(replay: &ReplayBatch, approved: &BTreeSet<String>) -> Result<()> {
    let mut tools = BTreeMap::<String, (String, String)>::new();
    for event in &replay.events {
        anyhow::ensure!(
            !matches!(event.kind, SessionEventKind::PayloadOmitted { .. }),
            "tool material was omitted from history"
        );
        let SessionEventKind::Update { update } = &event.kind else {
            continue;
        };
        let update = serde_json::to_value(update)?;
        if matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            if update["sessionUpdate"] == "tool_call" {
                anyhow::ensure!(
                    update["kind"].as_str().is_some(),
                    "initial tool kind is unspecified; local effects cannot be established"
                );
            }
            let id = update["toolCallId"]
                .as_str()
                .context("tool history omitted its identity")?;
            anyhow::ensure!(
                approved.contains(id),
                "tool execution has no matching scoped permission"
            );
            let tool = tools.entry(id.into()).or_default();
            if let Some(kind) = update["kind"].as_str() {
                anyhow::ensure!(
                    matches!(kind, "read" | "edit" | "search"),
                    "tool history includes nonlocal or unsupported effects"
                );
                tool.0 = kind.into();
            }
            if let Some(status) = update["status"].as_str() {
                tool.1 = status.into();
            }
        }
    }
    anyhow::ensure!(
        !tools.is_empty() && tools.len() == approved.len(),
        "finished local tool evidence is missing"
    );
    for (kind, status) in tools.values() {
        anyhow::ensure!(
            matches!(kind.as_str(), "read" | "edit" | "search")
                && matches!(status.as_str(), "completed" | "failed"),
            "tool effects are external, unsupported or still unresolved"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_local_kind_cannot_hide_prior_execute_or_fetch() -> Result<()> {
        for kind in ["execute", "fetch", "other", "edit"] {
            let replay: ReplayBatch = serde_json::from_value(serde_json::json!({
                "first_retained_seq":1,"snapshot_seq":2,"history_complete":true,"events":[
                    {"seq":1,"at":"2026-10-13T00:00:00Z","kind":{"event":"update","update":{
                        "sessionUpdate":"tool_call","toolCallId":"actual-tool","title":"Original operation","kind":kind,"status":"in_progress"}}},
                    {"seq":2,"at":"2026-10-13T00:00:01Z","kind":{"event":"update","update":{
                        "sessionUpdate":"tool_call_update","toolCallId":"actual-tool","kind":"edit","status":"completed"}}}
                ]
            }))?;
            assert_eq!(
                completed_local_tools(&replay, &BTreeSet::from(["actual-tool".into()])).is_ok(),
                kind == "edit",
                "original kind: {kind}"
            );
        }
        Ok(())
    }
}
