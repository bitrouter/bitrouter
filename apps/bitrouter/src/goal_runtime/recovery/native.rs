//! Reconcile native local effects from the original, bounded Thread history.
use super::*;
use bitrouter_ai::types::{Content, Role};
use bitrouter_orchestrator::item::{CallOrigin, CallRecord};
use bitrouter_orchestrator::store::EffectStatus;
use bitrouter_orchestrator::thread::{ThreadChange, ThreadHistoryRequest, ThreadTarget};
use bitrouter_orchestrator::turn::TurnLifecycle;

struct Call {
    step: String,
    record: CallRecord,
    launched: bool,
    effect: Option<EffectStatus>,
}

#[derive(Default)]
struct Effects {
    calls: BTreeMap<String, Call>,
    finished: Option<TurnStatus>,
}

impl Effects {
    fn observe(&mut self, turn: &str, change: &ThreadChange) -> Result<()> {
        match change {
            ThreadChange::AssistantResponse {
                turn_id,
                step_id,
                calls,
                ..
            } if turn_id == turn => {
                anyhow::ensure!(
                    self.finished.is_none(),
                    "native response follows terminal evidence"
                );
                for call in calls {
                    anyhow::ensure!(
                        call.origin == CallOrigin::Model
                            && !call.item_id.is_empty()
                            && !self.calls.contains_key(&call.item_id),
                        "native call identity is missing or reused"
                    );
                    self.calls.insert(
                        call.item_id.clone(),
                        Call {
                            step: step_id.clone(),
                            record: call.clone(),
                            launched: false,
                            effect: None,
                        },
                    );
                }
            }
            ThreadChange::ToolIntent {
                turn_id,
                step_id,
                call,
            } if turn_id == turn => {
                anyhow::ensure!(
                    self.finished.is_none(),
                    "native launch follows terminal evidence"
                );
                let known = self
                    .calls
                    .get_mut(&call.item_id)
                    .context("native intent has no original model call")?;
                anyhow::ensure!(
                    known.step == *step_id
                        && known.record.origin == call.origin
                        && known.record.provider_call_id == call.provider_call_id
                        && known.record.name == call.name
                        && serde_json::from_str::<serde_json::Value>(&known.record.arguments)?
                            == serde_json::from_str::<serde_json::Value>(&call.arguments)?
                        && !known.launched
                        && known.effect.is_none(),
                    "native intent differs from its original call or was repeated"
                );
                known.launched = true;
            }
            ThreadChange::ToolResult {
                turn_id,
                step_id,
                item_id,
                message,
                effect,
            } if turn_id == turn => {
                anyhow::ensure!(
                    self.finished.is_none(),
                    "native tool result follows terminal evidence"
                );
                let call = self
                    .calls
                    .get_mut(item_id)
                    .context("native tool result has no original call")?;
                anyhow::ensure!(
                    call.step == *step_id
                        && call.effect.is_none()
                        && *effect != EffectStatus::Unknown
                        && (call.launched || *effect == EffectStatus::NotExecuted)
                        && message.role == Role::Tool
                        && message.content.len() == 1
                        && matches!(message.content.first(), Some(Content::ToolResult { call_id, tool_name, .. })
                        if call_id == &call.record.provider_call_id && tool_name.as_ref().is_none_or(|name| name == &call.record.name)),
                    "native tool result is unresolved, duplicated or does not match its launch"
                );
                call.effect = Some(*effect);
            }
            ThreadChange::VerificationResult { turn_id, .. } if turn_id == turn => {
                anyhow::bail!(
                    "native verification command effects require separate reconciliation"
                );
            }
            ThreadChange::TurnLifecycle {
                turn_id,
                lifecycle:
                    TurnLifecycle::Finished {
                        status,
                        unknown_effect,
                        ..
                    },
            } if turn_id == turn => {
                anyhow::ensure!(
                    status.terminal() && !unknown_effect && self.finished.is_none(),
                    "native terminal evidence is missing, repeated or unknown"
                );
                self.finished = Some(*status);
            }
            _ => {}
        }
        Ok(())
    }

    async fn verify(&self, status: TurnStatus, workspace: &Path) -> Result<()> {
        anyhow::ensure!(
            self.finished == Some(status),
            "native terminal history does not match its snapshot"
        );
        for call in self.calls.values() {
            anyhow::ensure!(call.effect.is_some(), "native call has no settled result");
            let scope = crate::goal_permission_material::native::tool_scope(
                &call.record.name,
                &call.record.arguments,
            )
            .context(
                "native shell, MCP or unsupported operation needs explicit effect reconciliation",
            )?;
            anyhow::ensure!(
                permissions::is_local_tool(&scope, workspace).await?,
                "native operation is outside the owned local workspace"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

impl Coordinator {
    pub(super) async fn native_local_recovery_evidence(
        &self,
        goal: &GoalSnapshot,
        task: &Task,
        execution: &Execution,
    ) -> Result<(ArtifactRef, ArtifactRef)> {
        let binding = execution
            .thread
            .as_ref()
            .context("native work has no original Thread/Turn admission")?;
        let target = ThreadTarget {
            thread_id: binding.thread_id.clone(),
            server_instance_id: self.threads.capabilities().server_instance_id,
        };
        let caller = CallerContext::local();
        let view = self
            .threads
            .read_stored_thread_view(&target, &caller)
            .await?;
        anyhow::ensure!(
            view.thread.goal_work.as_ref()
                == Some(&crate::goal_runtime::native::source(goal, task, execution)?)
                && view.thread.workspace.canonicalize()? == execution.workspace.canonicalize()?
                && view.thread.active_turn_id.is_none()
                && view.thread.queued.is_empty(),
            "native source, workspace or active execution changed"
        );
        if let Some(recovery) = &view.recovery {
            anyhow::ensure!(
                recovery
                    .source_execution_owner
                    .as_ref()
                    .is_some_and(|owner| owner.server_instance_id
                        == recovery.source_server_instance_id
                        && owner.generation > 0
                        && owner.stopped_at_ms.is_some()),
                "native source owner has no confirmed shutdown boundary"
            );
        }
        let receipt = self
            .threads
            .find_goal_worker_turn(
                &target,
                &caller,
                &crate::goal_runtime::native::turn_key(&execution.id),
            )
            .await?
            .context("original native Turn admission receipt is unavailable")?;
        anyhow::ensure!(
            receipt.thread_id == binding.thread_id && receipt.turn_id == binding.turn_id,
            "native admission belongs to another execution"
        );
        let turn = self
            .threads
            .read_stored_turn(&target, &caller, &binding.turn_id)
            .await?;
        anyhow::ensure!(
            turn.status.terminal() && !turn.unknown_effect && turn.pending_input.is_none(),
            "native execution still has unresolved effects or input"
        );
        let cutoff = view.thread.cursor;
        anyhow::ensure!(
            cutoff > 0
                && cutoff
                    <= self
                        .threads
                        .capabilities()
                        .limits
                        .recovery_records_per_thread,
            "native history exceeds its existing recovery record bound"
        );
        let root = crate::goal_artifacts::root(&self.workspaces, &goal.id);
        let mut pages = Vec::new();
        let mut effects = Effects::default();
        let mut after = 0;
        loop {
            let page = self
                .threads
                .thread_history(
                    &target,
                    &caller,
                    ThreadHistoryRequest {
                        after,
                        cutoff: Some(cutoff),
                        limit: 128,
                    },
                )
                .await?;
            anyhow::ensure!(
                page.thread_id == binding.thread_id && page.cutoff == cutoff,
                "native history cutoff or Thread changed"
            );
            for event in &page.events {
                anyhow::ensure!(
                    event.thread_id == binding.thread_id
                        && event.seq > after
                        && event.seq <= cutoff,
                    "native history is unordered or mismatched"
                );
                for change in &event.changes {
                    effects.observe(&binding.turn_id, change)?;
                }
                after = event.seq;
            }
            pages.push(crate::goal_artifacts::put(&root, &serde_json::to_vec(&page)?).await?);
            match page.next_after {
                Some(next) => anyhow::ensure!(
                    next == after && !page.events.is_empty(),
                    "native history made no progress"
                ),
                None => break,
            }
        }
        anyhow::ensure!(after == cutoff, "native terminal history is incomplete");
        effects.verify(turn.status, &execution.workspace).await?;
        let base = execution
            .base_commit
            .as_deref()
            .context("native workspace has no committed baseline")?;
        let workspace = crate::goal_artifacts::capture(&root, &execution.workspace, base).await?;
        anyhow::ensure!(
            workspace == crate::goal_artifacts::capture(&root, &execution.workspace, base).await?,
            "native stopped workspace is still changing"
        );
        let again = self
            .threads
            .read_stored_thread_view(&target, &caller)
            .await?;
        let final_turn = self
            .threads
            .read_stored_turn(&target, &caller, &binding.turn_id)
            .await?;
        anyhow::ensure!(
            again.thread.cursor == cutoff
                && again.thread.goal_work == view.thread.goal_work
                && again.thread.active_turn_id.is_none()
                && again.thread.queued.is_empty()
                && final_turn.status == turn.status
                && !final_turn.unknown_effect
                && final_turn.pending_input.is_none(),
            "native execution changed during effect reconciliation"
        );
        let transcript = crate::goal_artifacts::put(&root, &serde_json::to_vec(&serde_json::json!({
            "scope":"Original native admission, complete Thread history and settled workspace-local tool effects. NotExecuted is not consent; failures can leave recorded local changes. This is replacement evidence, not completion, new permission or replay authority.",
            "execution_id":execution.id,"binding":binding,"source":view.thread.goal_work,
            "current_task_revision":task.requirements_revision,"stop":execution.stop,"snapshot":final_turn,
            "history_cutoff":cutoff,"history_pages":pages,"workspace":workspace,
        }))?).await?;
        Ok((workspace, transcript))
    }
}
