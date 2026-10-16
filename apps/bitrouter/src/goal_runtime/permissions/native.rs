//! Native choices use the original ThreadService approval transaction and key.
use super::*;
use crate::goal_permission_material::native::Request;
use bitrouter_orchestrator::goal::decision::native_choice;
use bitrouter_orchestrator::thread::ThreadTarget;
use bitrouter_orchestrator::turn::{ApprovalAnswer, TurnStatus};

fn answer(
    request: &Request,
    decision: &PermissionDecision,
    operation_id: &str,
    option_id: &str,
) -> Result<ApprovalAnswer> {
    let approved = native_choice(option_id).context("native permission option was not offered")?;
    use sha2::{Digest, Sha256};
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(
        &decision.id,
        operation_id,
    ))?));
    Ok(ApprovalAnswer {
        turn_id: request.native.turn_id.clone(),
        request_id: request.input.request_id.clone(),
        approved,
        idempotency_key: format!("goal-permission-{key}"),
    })
}

impl Coordinator {
    pub(super) async fn observe_native_permission(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<()> {
        let binding = execution
            .thread
            .as_ref()
            .context("native permission has no execution binding")?;
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: self.threads.capabilities().server_instance_id,
        };
        let turn = self
            .threads
            .read_stored_turn(&target, &CallerContext::local(), &binding.turn_id)
            .await?;
        if turn.status != TurnStatus::WaitingForInput {
            return Ok(());
        }
        let Some(input) = turn.pending_input else {
            return Ok(());
        };
        use sha2::{Digest, Sha256};
        let id = hex::encode(Sha256::digest(serde_json::to_vec(&(
            &execution.id,
            &binding,
            &input.request_id,
            goal.requirements_revision,
            task.requirements_revision,
        ))?));
        if goal.workflow.decisions.iter().any(|known| known.id == id) {
            return Ok(());
        }
        let material = Request {
            native: binding.clone(),
            input,
        };
        let bytes = serde_json::to_vec(&material)?;
        Request::decode(&bytes)?;
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let request = crate::goal_artifacts::put(&root, &bytes).await?;
        let options = ["allow-once", "deny-once"]
            .into_iter()
            .map(|id| PermissionOption {
                id: id.into(),
                label: if id == "allow-once" {
                    "Allow once"
                } else {
                    "Deny"
                }
                .into(),
                allows: native_choice(id) == Some(true),
                persistent: false,
            })
            .collect();
        self.change(
            goal,
            Change::Decision {
                event: Event::Observe {
                    decision: PermissionDecision {
                        id,
                        execution_id: execution.id.clone(),
                        run_id: None,
                        permission_id: material.input.request_id,
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

    async fn native_permission_request(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
    ) -> Result<(Request, ThreadTarget)> {
        anyhow::ensure!(
            decision.run_id.is_none(),
            "ACP permission cannot target a native input"
        );
        let task = goal
            .workflow
            .tasks
            .iter()
            .find(|task| {
                task.executions
                    .iter()
                    .any(|execution| execution.id == decision.execution_id)
            })
            .context("native permission task is missing")?;
        let execution = task
            .executions
            .iter()
            .find(|execution| execution.id == decision.execution_id)
            .context("native permission execution is missing")?;
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let request =
            Request::decode(&crate::goal_artifacts::read(&root, &decision.request).await?)?;
        anyhow::ensure!(
            task.spec.harness.is_none()
                && execution.thread.as_ref() == Some(&request.native)
                && decision.permission_id == request.input.request_id,
            "native request does not match its original execution/input"
        );
        let target = ThreadTarget {
            thread_id: request.native.thread_id.clone(),
            server_instance_id: self.threads.capabilities().server_instance_id,
        };
        let view = self
            .threads
            .read_stored_thread_view(&target, &CallerContext::local())
            .await?;
        anyhow::ensure!(
            view.thread
                .goal_work
                .as_ref()
                .is_some_and(|source| source.goal_id == goal.id
                    && source.execution_id == execution.id
                    && source.task_id == task.spec.id
                    && source.task_revision == execution.task_revision
                    && source.control_source_revision == execution.control_source_revision
                    && Some(&source.owner_thread_id) == goal.workflow.owner_thread_id.as_ref())
                && view.thread.workspace.canonicalize()? == execution.workspace.canonicalize()?,
            "native permission Thread has another source/workspace"
        );
        Ok((request, target))
    }

    pub(super) async fn validate_native_permission(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
    ) -> Result<()> {
        let (request, target) = self.native_permission_request(goal, decision).await?;
        let turn = self
            .threads
            .read_stored_turn(&target, &CallerContext::local(), &request.native.turn_id)
            .await?;
        anyhow::ensure!(
            turn.status == TurnStatus::WaitingForInput
                && turn.pending_input.as_ref() == Some(&request.input),
            "native permission is no longer pending with its original arguments"
        );
        Ok(())
    }

    pub(super) async fn native_permission_closed(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
    ) -> Result<bool> {
        let (request, target) = self.native_permission_request(goal, decision).await?;
        let turn = self
            .threads
            .read_stored_turn(&target, &CallerContext::local(), &request.native.turn_id)
            .await?;
        Ok(turn.status.terminal()
            && !turn.unknown_effect
            && turn.pending_input_id.as_deref() != Some(&request.input.request_id))
    }

    pub(super) async fn native_permission_selected(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
        operation_id: &str,
        option_id: &str,
    ) -> Result<bool> {
        let (request, target) = self.native_permission_request(goal, decision).await?;
        let answer = answer(&request, decision, operation_id, option_id)?;
        Ok(self
            .threads
            .find_input_answer(&target, &CallerContext::local(), &answer)
            .await?
            .is_some())
    }

    pub(super) async fn deliver_native_permission(
        &self,
        goal: &GoalSnapshot,
        decision: &PermissionDecision,
        operation_id: &str,
        option_id: &str,
    ) -> Result<bool> {
        // Reconcile first, even after a pause; a past accepted choice is not
        // new authority and does not prove that its tool ran.
        if self
            .native_permission_selected(goal, decision, operation_id, option_id)
            .await?
        {
            return Ok(true);
        }
        self.validate_native_permission(goal, decision).await?;
        let (request, target) = self.native_permission_request(goal, decision).await?;
        let answer = answer(&request, decision, operation_id, option_id)?;
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
            anyhow::bail!("native permission Goal is unavailable");
        };
        let task = current
            .workflow
            .tasks
            .iter()
            .find(|task| {
                task.executions
                    .iter()
                    .any(|execution| execution.id == decision.execution_id)
            })
            .context("native permission task disappeared")?;
        let execution = task
            .executions
            .iter()
            .find(|execution| execution.id == decision.execution_id)
            .context("native permission execution disappeared")?;
        let time = now()?;
        anyhow::ensure!(current.workflow.coordinator.as_ref().is_some_and(|owner| owner.id == self.id && owner.expires_at_ms > time)
            && current.requirements_revision == decision.requirements_revision && task.requirements_revision == decision.task_revision
            && execution.stop.is_none() && execution.stop_boundary_deadline_ms.is_none()
            && current.workflow.decisions.iter().any(|known| known.id == decision.id
                && matches!(&known.state, PermissionState::Requested { operation_id: op, option_id: option, dispatch_requested: true, .. }
                    if op == operation_id && option == option_id)), "native reply source, owner or intent changed before delivery");
        if answer.approved {
            anyhow::ensure!(
                current.run_intent == RunIntent::Run
                    && task.run_intent == RunIntent::Run
                    && current
                        .control_source_revision
                        .max(task.control_source_revision)
                        == execution.control_source_revision,
                "native allow is stale or its work is paused"
            );
            current.budget.admission(current.created_at_ms, now()?)?;
        }
        let result = self
            .threads
            .answer_thread_input(&target, &CallerContext::local(), answer)
            .await;
        let recorded = self
            .native_permission_selected(&current, decision, operation_id, option_id)
            .await?;
        if !recorded {
            result?;
        }
        Ok(recorded)
    }
}
