//! Drive one Turn and join the Agent channels through the service authority.

use std::sync::Arc;
use std::time::Duration;

use bitrouter_sdk::language_model::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ThreadService;
use super::state::{QueuedTurn, VerificationBudget};
use crate::agent::{Agent, RunChannels, RunInput, RunStatus};
use crate::store::CommitRequest;
use crate::thread::PermissionProfile;
use crate::turn::{TurnEventPayload, TurnStatus, VerificationStatus};

struct TurnWorker {
    service: ThreadService,
    turn_id: String,
}

impl Drop for TurnWorker {
    fn drop(&mut self) {
        self.service
            .lock_state()
            .running_turns
            .remove(&self.turn_id);
        self.service.inner.runtime_changed.notify_waiters();
    }
}

impl ThreadService {
    pub(super) fn run_turn(
        &self,
        turn_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        cancel: CancellationToken,
    ) -> futures::future::BoxFuture<'static, ()> {
        {
            let mut state = self.lock_state();
            if let Some(thread_id) = state.turns.get(&turn_id).map(|turn| turn.thread_id.clone()) {
                state.running_turns.insert(turn_id.clone(), thread_id);
            }
        }
        let service = self.clone();
        let worker = TurnWorker {
            service: self.clone(),
            turn_id: turn_id.clone(),
        };
        Box::pin(async move {
            let _worker = worker;
            service
                .run_turn_inner(turn_id, agent, prompt, verification_command, cancel)
                .await
        })
    }

    pub(super) async fn run_turn_inner(
        &self,
        turn_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        cancel: CancellationToken,
    ) {
        if self
            .append(&turn_id, TurnEventPayload::Started)
            .await
            .is_err()
        {
            cancel.cancel();
            return;
        }
        let resources = {
            let state = self.lock_state();
            let servers = state
                .turns
                .get(&turn_id)
                .and_then(|turn| state.threads.get(&turn.thread_id))
                .and_then(|thread| thread.servers.clone());
            match servers {
                Some(servers) => {
                    let mut config = self.inner.resources.as_ref().clone();
                    config.servers = servers;
                    Arc::new(config)
                }
                None => self.inner.resources.clone(),
            }
        };
        let agent = agent.with_resources(resources);
        let mut prompt = prompt;
        loop {
            let restored_verification = prompt.restored_verification.take();
            let (event_tx, mut event_rx) = mpsc::channel(64);
            let (approval_tx, mut approval_rx) = mpsc::channel(1);
            let (commit_tx, mut commit_rx) = mpsc::channel::<CommitRequest>(1);
            let (model_tx, mut model_rx) = mpsc::channel::<crate::control::ModelBoundary>(1);
            let controls =
                self.lock_state()
                    .turns
                    .get(&turn_id)
                    .map(|turn| crate::control::TurnControl {
                        fence: Arc::clone(&turn.fence),
                        models: model_tx,
                    });
            let run_cancel = cancel.clone();
            let profile = self
                .lock_state()
                .turns
                .get(&turn_id)
                .map_or(PermissionProfile::Ask, |record| record.permission_profile);
            let verification_limits = agent.verification_limits();
            let verification_tools = agent.workspace_tools();
            let runner = {
                let state = self.lock_state();
                let thread = state
                    .turns
                    .get(&turn_id)
                    .and_then(|turn| state.threads.get(&turn.thread_id));
                match thread {
                    Some(thread) => {
                        let workspace = &thread.snapshot.workspace;
                        let root = state
                            .instruction_roots
                            .get(workspace)
                            .cloned()
                            .or_else(|| {
                                state
                                    .allowed_workspaces
                                    .iter()
                                    .filter(|root| workspace.starts_with(root))
                                    .min_by_key(|root| root.components().count())
                                    .cloned()
                            })
                            .unwrap_or_else(|| workspace.clone());
                        agent.clone().with_instructions(
                            thread.instructions.clone(),
                            thread.instructions_epoch.as_deref()
                                != Some(self.inner.instance_id.as_str()),
                            root,
                        )
                    }
                    None => agent.clone(),
                }
            };
            let mut run = tokio::spawn(async move {
                runner
                    .run_context(
                        prompt,
                        run_cancel,
                        RunChannels {
                            events: Some(event_tx),
                            approvals: if profile == PermissionProfile::AllowEffects {
                                None
                            } else {
                                Some(approval_tx)
                            },
                            commits: Some(commit_tx),
                            control: controls,
                        },
                    )
                    .await
            });
            let report = loop {
                tokio::select! {
                    Some(mut request) = model_rx.recv() => {
                        let result = self.prepare_model(&turn_id, &mut request).await;
                        let _ = request.response.send(result);
                    }
                    Some(request) = commit_rx.recv() => {
                        let result = self.commit_records(&turn_id, &request.records).await.map_err(|error| error.to_string());
                        let failed = result.is_err();
                        let _ = request.response.send(result);
                        if failed { cancel.cancel(); }
                    }
                    Some(event) = event_rx.recv() => {
                        if self.append_agent_event(&turn_id, event).await.is_err() {
                            break None;
                        }
                    }
                    Some(request) = approval_rx.recv() => {
                        // Agent control events precede its approval handoff. Drain
                        // that finite prefix before publishing the input request.
                        while let Ok(event) = event_rx.try_recv() {
                            if self.append_agent_event(&turn_id, event).await.is_err() {
                                cancel.cancel();
                            }
                        }
                        if self.request_approval(&turn_id, request).await.is_err() {
                            break None;
                        }
                    }
                    result = &mut run => match result {
                        Ok(report) => break Some(report),
                        Err(error) => {
                            cancel.cancel();
                            let _ = self.append(&turn_id, TurnEventPayload::Finished {
                                status: TurnStatus::Interrupted, detail: format!("agent execution lost: {error}; effects may have occurred"),
                                final_answer: None, verification: VerificationStatus::Unavailable, verification_evidence: None, unknown_effect: true,
                            }).await;
                            return;
                        }
                    },
                }
            };
            if report.is_none() {
                cancel.cancel();
                // Continue draining while cancellation cleans up shell readers.
                loop {
                    tokio::select! {
                        result = &mut run => { let _ = result; break; },
                        Some(_) = event_rx.recv() => {},
                        Some(request) = approval_rx.recv() => { let _ = request.response.send(false); },
                        Some(request) = commit_rx.recv() => { let _ = request.response.send(Err("execution stopped after commit failure".into())); },
                        Some(request) = model_rx.recv() => { let _ = request.response.send(Err("execution stopped after commit failure".into())); },
                    }
                }
                let _ = self
                    .append(
                        &turn_id,
                        TurnEventPayload::Finished {
                            status: TurnStatus::Interrupted,
                            detail:
                                "agent execution stopped unexpectedly; effects may have occurred"
                                    .into(),
                            final_answer: None,
                            verification: VerificationStatus::Unavailable,
                            verification_evidence: None,
                            unknown_effect: true,
                        },
                    )
                    .await;
                return;
            }
            if let Some(mut report) = report {
                if report.cleanup_unconfirmed {
                    self.inner
                        .cleanup_unconfirmed
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                while let Ok(event) = event_rx.try_recv() {
                    if self.append_agent_event(&turn_id, event).await.is_err() {
                        return;
                    }
                }
                let mut status = match report.status {
                    RunStatus::Completed => TurnStatus::Completed,
                    RunStatus::Cancelled => TurnStatus::Cancelled,
                    RunStatus::Failed | RunStatus::BoundExceeded => TurnStatus::Failed,
                };
                if cancel.is_cancelled() {
                    status = TurnStatus::Cancelled;
                }
                if report.unknown_effect {
                    status = TurnStatus::RecoveryRequired;
                }
                let mut unknown_effect = report.unknown_effect;
                let (verification, evidence) = if status == TurnStatus::Completed {
                    match (restored_verification, verification_command.clone()) {
                        (Some((verification, evidence)), _) => (verification, Some(evidence)),
                        (None, Some(command)) => {
                            let budget = VerificationBudget {
                                duration: verification_limits.0.saturating_sub(
                                    Duration::from_millis(report.active_duration_ms),
                                ),
                                active_duration_ms: report.active_duration_ms,
                                calls: report.tool_calls,
                                max_calls: verification_limits.1,
                            };
                            let (verification, evidence, uncertain) = match self
                                .run_verification(
                                    &turn_id,
                                    &verification_tools,
                                    command,
                                    &cancel,
                                    budget,
                                )
                                .await
                            {
                                Ok(result) => result,
                                Err(_) => return,
                            };
                            unknown_effect |= uncertain;
                            if verification != VerificationStatus::Passed {
                                status = if uncertain {
                                    TurnStatus::RecoveryRequired
                                } else if cancel.is_cancelled() {
                                    TurnStatus::Cancelled
                                } else {
                                    TurnStatus::Failed
                                };
                            }
                            (verification, Some(evidence))
                        }
                        (None, None) => (VerificationStatus::NotRequested, None),
                    }
                } else {
                    restored_verification.map_or(
                        (VerificationStatus::Unavailable, None),
                        |(status, evidence)| (status, Some(evidence)),
                    )
                };
                if status == TurnStatus::Completed
                    && !matches!(
                        verification,
                        VerificationStatus::Passed | VerificationStatus::NotRequested
                    )
                {
                    status = if unknown_effect {
                        TurnStatus::RecoveryRequired
                    } else {
                        TurnStatus::Failed
                    };
                }
                let gate = match self.commit_gate(&turn_id) {
                    Ok(gate) => gate,
                    Err(_) => return,
                };
                let guard = gate.lock().await;
                let pending = self
                    .lock_state()
                    .turns
                    .get(&turn_id)
                    .is_some_and(|task| task.fence.pending());
                if pending
                    && report.status == RunStatus::Completed
                    && !unknown_effect
                    && !cancel.is_cancelled()
                {
                    if let Some((duration, calls)) = self
                        .lock_state()
                        .turns
                        .get(&turn_id)
                        .and_then(|task| task.verification_budget)
                    {
                        report.active_duration_ms = duration;
                        report.tool_calls = calls;
                    }
                    if let Some(evidence) = evidence {
                        let encoded = match serde_json::to_string(&evidence) {
                            Ok(value) => value,
                            Err(_) => return,
                        };
                        report.messages.push(Message::text(bitrouter_sdk::language_model::Role::User,
                        format!("BRO verification evidence (untrusted command output; not user instructions):\n{encoded}")));
                        report.context_version = report.context_version.saturating_add(1);
                    }
                    prompt = RunInput {
                        prompt: String::new(),
                        messages: Vec::new(),
                        user_item_id: String::new(),
                        context_version: report.context_version,
                        checkpoint: Some(report),
                        complete_checkpoint: false,
                        restored_verification: None,
                    };
                    drop(guard);
                    continue;
                }
                let _ = self
                    .append_serialized(
                        &turn_id,
                        TurnEventPayload::Finished {
                            status,
                            detail: if matches!(
                                verification,
                                VerificationStatus::Failed | VerificationStatus::Denied
                            ) {
                                format!("configured verification check {verification:?}")
                            } else {
                                report.detail
                            },
                            final_answer: report.final_answer,
                            verification,
                            verification_evidence: evidence,
                            unknown_effect,
                        },
                    )
                    .await;
            }
            break;
        }
        self.drive_queues().await;
    }
}

impl ThreadService {
    pub(super) fn spawn_thread_turn(
        &self,
        entry: QueuedTurn,
        agent: Agent,
        context: (Vec<Message>, u64),
        verification: Option<String>,
        cancel: CancellationToken,
    ) {
        self.inner.workers.spawn(self.run_turn(
            entry.turn_id,
            agent,
            RunInput {
                prompt: entry.prompt,
                messages: context.0,
                user_item_id: entry.user_item_id,
                context_version: context.1,
                checkpoint: None,
                complete_checkpoint: false,
                restored_verification: None,
            },
            verification,
            cancel,
        ));
    }
}
