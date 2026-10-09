//! Run configured verification with the same approvals, budget and effect protocol.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_ai::types::ToolResultOutput;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::state::VerificationBudget;
use super::{ServiceError, ThreadService, unknown_turn};
use crate::agent::ApprovalRequest;
use crate::item::{CallOrigin, CallRecord};
use crate::store::{EffectStatus, ExecutionRecord};
use crate::thread::PermissionProfile;
use crate::tools::WorkspaceTools;
use crate::turn::{VerificationEvidence, VerificationStatus};

fn verification_evidence(command: String, result: &ToolResultOutput) -> VerificationEvidence {
    let mut evidence = VerificationEvidence {
        command: command.clone(),
        interpreter: None,
        exit_status: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
        error: None,
    };
    match result {
        ToolResultOutput::Json { value } => {
            evidence.interpreter = value.get("interpreter").cloned();
            evidence.exit_status = value
                .get("exit_status")
                .and_then(serde_json::Value::as_i64)
                .and_then(|status| i32::try_from(status).ok());
            evidence.stdout = value
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into();
            evidence.stderr = value
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into();
            evidence.stdout_truncated = value
                .get("stdout_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            evidence.stderr_truncated = value
                .get("stderr_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            evidence.timed_out = value
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        }
        ToolResultOutput::ErrorJson { value } => {
            evidence.error = Some(
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("verification command failed before execution")
                    .into(),
            );
        }
        ToolResultOutput::ExecutionDenied { reason } => evidence.error = reason.clone(),
        _ => evidence.error = Some("unexpected verification output".into()),
    }
    evidence
}

impl ThreadService {
    pub(super) async fn run_verification(
        &self,
        turn_id: &str,
        tools: &WorkspaceTools,
        command: String,
        cancel: &CancellationToken,
        budget: VerificationBudget,
    ) -> Result<(VerificationStatus, VerificationEvidence, bool), ServiceError> {
        let fence = self
            .lock_state()
            .turns
            .get(turn_id)
            .map(|task| Arc::clone(&task.fence))
            .ok_or_else(unknown_turn)?;
        let name = "shell";
        let arguments = serde_json::json!({"command":command}).to_string();
        WorkspaceTools::validate(name, &arguments)?;
        let call = CallRecord {
            origin: CallOrigin::Verification,
            item_id: uuid::Uuid::new_v4().to_string(),
            provider_call_id: String::new(),
            name: name.into(),
            arguments,
        };
        let started = Instant::now();
        let mut approval_wait = Duration::ZERO;
        let mut effect = EffectStatus::NotExecuted;
        let mut expired = false;
        let output = 'verification: {
            if fence.pending() {
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                }
            } else if budget.duration.is_zero()
                || budget.calls >= budget.max_calls
                || cancel.is_cancelled()
            {
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"not_executed":true,"error":"verification cannot start within the remaining execution budget"}),
                }
            } else {
                let allow_effects =
                    self.lock_state().turns.get(turn_id).is_some_and(|task| {
                        task.permission_profile == PermissionProfile::AllowEffects
                    });
                let approved = if allow_effects {
                    true
                } else {
                    let (response, receiver) = oneshot::channel();
                    let wait_started = Instant::now();
                    self.request_approval(
                        turn_id,
                        ApprovalRequest {
                            id: uuid::Uuid::new_v4().to_string(),
                            tool_id: call.item_id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            response,
                        },
                    )
                    .await?;
                    let approved = tokio::select! { biased; _ = cancel.cancelled() => false, result = receiver => result.unwrap_or(false) };
                    approval_wait += wait_started.elapsed();
                    approved
                };
                if cancel.is_cancelled() {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"not_executed":true,"error":"verification cancelled before execution"}),
                    }
                } else if fence.pending() {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                    }
                } else if !approved {
                    ToolResultOutput::ExecutionDenied {
                        reason: Some("verification denied".into()),
                    }
                } else {
                    let remaining = budget
                        .duration
                        .saturating_sub(started.elapsed().saturating_sub(approval_wait));
                    let permit = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        _ = fence.received() => None,
                        result = tokio::time::timeout(remaining, Arc::clone(&self.inner.tool_workers).acquire_owned()) => result.ok().and_then(Result::ok),
                    };
                    if let Some(permit) = permit {
                        if cancel.is_cancelled()
                            || started.elapsed().saturating_sub(approval_wait) >= budget.duration
                        {
                            ToolResultOutput::ErrorJson {
                                value: serde_json::json!({"not_executed":true,"error":"verification stopped before execution"}),
                            }
                        } else {
                            let tools = tools.clone();
                            self.commit_records(
                                turn_id,
                                &[ExecutionRecord::ToolIntent {
                                    step_id: uuid::Uuid::new_v4().to_string(),
                                    call: call.clone(),
                                }],
                            )
                            .await?;
                            if cancel.is_cancelled()
                                || started.elapsed().saturating_sub(approval_wait)
                                    >= budget.duration
                            {
                                break 'verification ToolResultOutput::ErrorJson {
                                    value: serde_json::json!({"not_executed":true,"error":"verification stopped after intent commit"}),
                                };
                            }
                            let worker_cancel = cancel.child_token();
                            let (events, mut receiver) = mpsc::channel(64);
                            let tool_cancel = worker_cancel.clone();
                            let arguments = call.arguments.clone();
                            let item_id = call.item_id.clone();
                            let dispatched = fence.launch(|| {
                                let (start, ready) = oneshot::channel::<()>();
                                let run = tokio::spawn(async move {
                                    let _permit = permit;
                                    if ready.await.is_err() { return (ToolResultOutput::ErrorJson {
                                        value: serde_json::json!({"execution_status":"not_executed","error":"verification start withdrawn"}),
                                    }, EffectStatus::NotExecuted); }
                                    tools.execute_with_effect(name, &arguments, &tool_cancel, &item_id, Some(&events)).await
                                });
                                (start, run)
                            });
                            let Some((start, mut run)) = dispatched else {
                                break 'verification ToolResultOutput::ErrorJson {
                                    value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                                };
                            };
                            let _ = start.send(());
                            let remaining = budget
                                .duration
                                .saturating_sub(started.elapsed().saturating_sub(approval_wait));
                            let deadline = tokio::time::sleep(remaining);
                            tokio::pin!(deadline);
                            let mut storage_error = None;
                            let result = loop {
                                tokio::select! {
                                    result = &mut run => break result.unwrap_or_else(|error| (ToolResultOutput::ErrorJson {
                                        value: serde_json::json!({"error":format!("verification worker lost: {error}"),"worker_lost":true}),
                                    }, EffectStatus::Unknown)),
                                    _ = &mut deadline, if !expired => { expired = true; worker_cancel.cancel(); },
                                    Some(event) = receiver.recv() => if storage_error.is_none() && let Err(error) = self.append_agent_event(turn_id, event).await { storage_error = Some(error); worker_cancel.cancel(); },
                                }
                            };
                            while let Ok(event) = receiver.try_recv() {
                                if storage_error.is_none()
                                    && let Err(error) =
                                        self.append_agent_event(turn_id, event).await
                                {
                                    storage_error = Some(error);
                                }
                            }
                            if let Some(error) = storage_error {
                                return Err(error);
                            }
                            effect = result.1;
                            result.0
                        }
                    } else {
                        ToolResultOutput::ErrorJson {
                            value: serde_json::json!({"not_executed":true,"error":"verification worker cancelled, unavailable or time bound reached"}),
                        }
                    }
                }
            }
        };
        let mut evidence = verification_evidence(command, &output);
        evidence.timed_out |= expired;
        let verification = if matches!(output, ToolResultOutput::ExecutionDenied { .. }) {
            VerificationStatus::Denied
        } else if effect == EffectStatus::NotExecuted {
            VerificationStatus::Unavailable
        } else if evidence.exit_status == Some(0) && !evidence.timed_out && evidence.error.is_none()
        {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        };
        let active_duration_ms = budget.active_duration_ms.saturating_add(
            u64::try_from(started.elapsed().saturating_sub(approval_wait).as_millis())
                .unwrap_or(u64::MAX),
        );
        self.commit_records(
            turn_id,
            &[ExecutionRecord::VerificationResult {
                status: Some(verification),
                call: call.clone(),
                evidence: evidence.clone(),
                effect,
                active_duration_ms,
                tool_calls: budget
                    .calls
                    .saturating_add(u32::from(budget.calls < budget.max_calls)),
            }],
        )
        .await?;
        Ok((verification, evidence, effect == EffectStatus::Unknown))
    }
}
