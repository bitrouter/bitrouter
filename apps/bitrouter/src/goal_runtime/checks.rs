use super::*;
use bitrouter_orchestrator::goal::verification::{CheckOutput, CheckSpec};
use bitrouter_orchestrator::goal::workflow::CheckState;

impl Coordinator {
    pub(super) async fn ensure_checks(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
        workspace: &ArtifactRef,
    ) -> Result<Vec<serde_json::Value>> {
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let mut evidence = Vec::new();
        for spec in &task.spec.checks {
            let existing = execution.checks.iter().find(|check| {
                check.spec == *spec
                    && check.task_revision == task.requirements_revision
                    && check.workspace == *workspace
            });
            if let Some(check) = existing {
                match &check.state {
                    CheckState::Observed {
                        report,
                        passed: true,
                        completed: true,
                    } => {
                        let output: CheckOutput = serde_json::from_slice(
                            &crate::goal_artifacts::read(&root, report).await?,
                        )?;
                        evidence.push(serde_json::json!({"check":spec,"artifact":workspace,"report":report,"actual_output":output}));
                        continue;
                    }
                    CheckState::Observed { .. } => {
                        anyhow::bail!("required check {} did not pass for this artifact", spec.id)
                    }
                    CheckState::Requested => {
                        let output = unavailable(
                            "Check admission survived without an observed outcome; its process/effects remain unknown and were not replayed",
                        );
                        let report =
                            crate::goal_artifacts::put(&root, &serde_json::to_vec(&output)?)
                                .await?;
                        self.change(
                            goal,
                            Change::ObserveCheck {
                                execution_id: execution.id.clone(),
                                attempt_id: check.id.clone(),
                                report,
                                output,
                                workspace_after: workspace.clone(),
                            },
                        )
                        .await?;
                        anyhow::bail!("check {} requires reconciliation", spec.id);
                    }
                }
            }
            let attempt_id = uuid::Uuid::new_v4().to_string();
            self.change(
                goal,
                Change::RequestCheck {
                    execution_id: execution.id.clone(),
                    attempt_id: attempt_id.clone(),
                    check_id: spec.id.clone(),
                    workspace: workspace.clone(),
                },
            )
            .await?;
            let output = self
                .run_workspace_check(
                    goal,
                    &execution.workspace,
                    Some((&task.spec.id, task.requirements_revision)),
                    spec,
                    &attempt_id,
                )
                .await
                .unwrap_or_else(|error| unavailable(&bounded_error(&error)));
            let report = crate::goal_artifacts::put(&root, &serde_json::to_vec(&output)?).await?;
            let after = crate::goal_artifacts::capture(
                &root,
                &execution.workspace,
                execution
                    .base_commit
                    .as_deref()
                    .context("check has no workspace baseline")?,
            )
            .await?;
            self.change(
                goal,
                Change::ObserveCheck {
                    execution_id: execution.id.clone(),
                    attempt_id,
                    report: report.clone(),
                    output: output.clone(),
                    workspace_after: after.clone(),
                },
            )
            .await?;
            anyhow::ensure!(
                output.supports(spec) && after == *workspace,
                "required check {} failed, remained uncertain, or changed its artifact",
                spec.id
            );
            evidence.push(serde_json::json!({"check":spec,"artifact":workspace,"report":report,"actual_output":output}));
        }
        Ok(evidence)
    }

    pub(super) async fn run_workspace_check(
        &self,
        goal: &GoalSnapshot,
        workspace: &Path,
        task: Option<(&str, u64)>,
        spec: &CheckSpec,
        attempt_id: &str,
    ) -> Result<CheckOutput> {
        let cancellation = CancellationToken::new();
        let future = bitrouter_orchestrator::goal::verification::run(
            workspace,
            spec,
            attempt_id,
            &cancellation,
        );
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                result = &mut future => return result.map_err(anyhow::Error::msg),
                _ = tick.tick() => {
                    let time = now()?;
                    let result = self.goals.execute("local", GoalCommand::Get { goal_id: goal.id.clone() }, time).await;
                    let allowed = match result {
                        Ok(GoalReply::Snapshot { goal: current }) => current.run_intent == RunIntent::Run
                            && current.budget.admission(current.created_at_ms, time).is_ok()
                            && current.requirements_revision == goal.requirements_revision
                            && task.is_none_or(|(id, revision)| current.workflow.tasks.iter().find(|current| current.spec.id == id).is_some_and(|current| current.run_intent == RunIntent::Run && current.requirements_revision == revision))
                            && current.workflow.coordinator.as_ref().is_some_and(|lease| lease.id == self.id && lease.expires_at_ms > time),
                        _ => false,
                    };
                    if !allowed { cancellation.cancel(); }
                }
            }
        }
    }
}

pub(super) fn unavailable(reason: &str) -> CheckOutput {
    CheckOutput {
        completed: false,
        passed: false,
        exit_status: None,
        timed_out: false,
        environment: serde_json::json!({"source":"local_bro_check","scope":"unavailable"}),
        output: serde_json::json!({"error":reason}),
    }
}
