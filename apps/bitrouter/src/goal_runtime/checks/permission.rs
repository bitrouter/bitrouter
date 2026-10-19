//! Explicit check consent is consumed by the live check job, never replayed.

use super::*;
use bitrouter_orchestrator::goal::decision::{
    Event, PermissionDecision, PermissionOption, PermissionState,
};
use bitrouter_orchestrator::goal::verification::permission::{
    CheckPermission, CommandPermit, authorize,
};

impl Coordinator {
    async fn check_snapshot(&self, goal_id: &str) -> Result<GoalSnapshot> {
        let GoalReply::Snapshot { goal } = self
            .goals
            .execute(
                "local",
                GoalCommand::Get {
                    goal_id: goal_id.into(),
                },
                now()?,
            )
            .await?
        else {
            anyhow::bail!("command-check Goal unavailable")
        };
        Ok(*goal)
    }

    pub(super) async fn check_permission(
        &self,
        goal: &GoalSnapshot,
        attempt_id: &str,
        baseline: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<CommandPermit> {
        let current = self.check_snapshot(&goal.id).await?;
        anyhow::ensure!(
            current.requirements_revision == goal.requirements_revision,
            "command-check requirements changed before permission admission"
        );
        let (execution_id, task_revision, control_revision, workspace, check) =
            if let Some((task, execution, check)) = current.workflow.tasks.iter().find_map(|task| {
                let execution = task.executions.last()?;
                let check = execution
                    .checks
                    .iter()
                    .find(|check| check.id == attempt_id)?;
                Some((task, execution, check))
            }) {
                (
                    execution.id.clone(),
                    task.requirements_revision,
                    current
                        .control_source_revision
                        .max(task.control_source_revision),
                    execution.workspace.clone(),
                    check.clone(),
                )
            } else {
                let attempt = current
                    .workflow
                    .completion
                    .attempt
                    .as_ref()
                    .context("command-check attempt missing")?;
                let check = attempt
                    .checks
                    .iter()
                    .find(|check| check.id == attempt_id)
                    .context("command-check admission missing")?;
                (
                    attempt.id.clone(),
                    0,
                    current.control_source_revision,
                    attempt.workspace.clone(),
                    check.clone(),
                )
            };
        let scope = CheckPermission {
            specification: check.spec,
            workspace,
            artifact: check.workspace,
            coordinator_id: self.id.clone(),
            control_source_revision: control_revision,
        };
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let decision = PermissionDecision {
            id: format!("check-{attempt_id}"),
            execution_id,
            run_id: None,
            check: Some(Box::new(scope.clone())),
            permission_id: attempt_id.into(),
            requirements_revision: current.requirements_revision,
            task_revision,
            request: crate::goal_artifacts::put(&root, &serde_json::to_vec(&scope)?).await?,
            options: vec![
                PermissionOption {
                    id: "allow-once".into(),
                    label: "Run this command once".into(),
                    allows: true,
                    persistent: false,
                },
                PermissionOption {
                    id: "deny-once".into(),
                    label: "Deny".into(),
                    allows: false,
                    persistent: false,
                },
            ],
            state: PermissionState::Pending,
        };
        scope.validate(&current, &decision)?;
        self.change(
            &current,
            Change::Decision {
                event: Event::Observe {
                    decision: Box::new(decision.clone()),
                },
            },
        )
        .await?;
        let result = self
            .wait_check_permission(&decision, &current.id, baseline, cancellation)
            .await;
        if result.is_err() {
            // Retire a still-pending card after cancellation/staleness. A
            // selected choice remains in the receipt and is not reusable.
            let latest = self.check_snapshot(&goal.id).await?;
            if latest.workflow.decisions.iter().any(|known| known.id == decision.id && known.state == PermissionState::Pending)
                && let Err(error) = self.change(&latest, Change::Decision { event: Event::Obsolete {
                    decision_id: decision.id,
                    reason: "Command check stopped before dispatch; this permission cannot be reused".into(),
                }}).await
            {
                tracing::warn!(%error, "could not retire stopped command-check permission");
            }
        }
        result
    }

    async fn wait_check_permission(
        &self,
        decision: &PermissionDecision,
        goal_id: &str,
        baseline: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<CommandPermit> {
        let scope = decision
            .check
            .as_ref()
            .context("command-check scope missing")?;
        let root = crate::goal_artifacts::root(&self.workspaces, goal_id);
        loop {
            anyhow::ensure!(
                !cancellation.is_cancelled(),
                "command check cancelled before permission"
            );
            let current = self.check_snapshot(goal_id).await?;
            scope.validate(&current, decision)?;
            let time = now()?;
            current.budget.admission(current.created_at_ms, time)?;
            anyhow::ensure!(
                current
                    .workflow
                    .coordinator
                    .as_ref()
                    .is_some_and(|lease| lease.id == self.id && lease.expires_at_ms > time),
                "command-check owner expired"
            );
            let live = current
                .workflow
                .decisions
                .iter()
                .find(|known| known.id == decision.id)
                .context("command-check permission disappeared")?;
            match &live.state {
                PermissionState::Pending => {}
                PermissionState::Requested {
                    operation_id,
                    option_id,
                    automatic: false,
                    dispatch_requested: false,
                } => {
                    let after = crate::goal_artifacts::capture(
                        &root,
                        &scope.workspace,
                        baseline.context("command-check baseline missing")?,
                    )
                    .await?;
                    anyhow::ensure!(
                        after == scope.artifact,
                        "command-check artifact changed while awaiting approval"
                    );
                    self.change(
                        &current,
                        Change::Decision {
                            event: Event::RequestDelivery {
                                decision_id: decision.id.clone(),
                                operation_id: operation_id.clone(),
                            },
                        },
                    )
                    .await?;
                    let admitted = self.check_snapshot(goal_id).await?;
                    let permit = if option_id == "allow-once" {
                        Some(authorize(&admitted, &decision.id, now()?)?)
                    } else {
                        None
                    };
                    self.change(&admitted, Change::Decision { event: Event::Result {
                        decision_id: decision.id.clone(), operation_id: operation_id.clone(), submitted: true,
                        detail: "One-time command-check choice consumed; execution outcome is recorded separately".into(),
                    }}).await?;
                    // Persisting consent is not a cached authorization. Refresh
                    // controls/ownership after that await and before spawn.
                    let after = crate::goal_artifacts::capture(
                        &root,
                        &scope.workspace,
                        baseline.context("command-check baseline missing")?,
                    )
                    .await?;
                    anyhow::ensure!(
                        after == scope.artifact,
                        "command-check artifact changed before execution"
                    );
                    let latest = self.check_snapshot(goal_id).await?;
                    scope.validate(&latest, decision)?;
                    let time = now()?;
                    latest.budget.admission(latest.created_at_ms, time)?;
                    anyhow::ensure!(
                        latest
                            .workflow
                            .coordinator
                            .as_ref()
                            .is_some_and(|lease| lease.id == self.id && lease.expires_at_ms > time),
                        "command-check owner expired before execution"
                    );
                    anyhow::ensure!(
                        !cancellation.is_cancelled(),
                        "command check cancelled before execution"
                    );
                    return permit.context("command check denied");
                }
                _ => anyhow::bail!(
                    "command-check choice is stale or was already consumed; no replay"
                ),
            }
            tokio::select! {
                _ = cancellation.cancelled() => anyhow::bail!("command check cancelled while awaiting permission"),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
}
