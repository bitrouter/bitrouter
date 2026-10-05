//! Bounded read groups and exclusive tools; all owned workers settle before returning.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use bitrouter_sdk::language_model::{Content, Message, ProviderMetadata, Role, ToolResultOutput};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Agent, RunEvent, RunReport, RunStatus, commit_execution, not_executed, record};
use crate::control::TurnControl;
use crate::item::CallRecord;
use crate::store::{CommitRequest, EffectStatus, ExecutionRecord};
use crate::tools::WorkspaceTools;

pub(super) struct PendingCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
    pub(super) provider_metadata: ProviderMetadata,
}

pub(super) struct Invocation {
    pub(super) call: PendingCall,
    pub(super) record: CallRecord,
}

pub(super) struct BatchControl<'a> {
    pub(super) cancel: &'a CancellationToken,
    pub(super) events: &'a Option<mpsc::Sender<RunEvent>>,
    pub(super) commits: &'a Option<mpsc::Sender<CommitRequest>>,
    pub(super) started: Instant,
    pub(super) steering: &'a Option<TurnControl>,
}

pub(super) struct BatchOutcome {
    pub(super) stop: Option<(RunStatus, String)>,
    pub(super) ordinary_error: bool,
}

impl Agent {
    pub(super) async fn execute_exclusive(
        &self,
        step_id: &str,
        call: &PendingCall,
        call_record: &CallRecord,
        report: &mut RunReport,
        control: BatchControl<'_>,
    ) -> Result<(ToolResultOutput, EffectStatus), String> {
        let permit = match self.acquire_worker(&control).await {
            Ok(permit) => permit,
            Err(error) => return Ok((not_executed(&error), EffectStatus::NotExecuted)),
        };
        if let Some((_, reason)) = self.effect_bound_status(report, control.started, control.cancel)
        {
            return Ok((not_executed(&reason), EffectStatus::NotExecuted));
        }
        commit_execution(
            control.commits,
            vec![ExecutionRecord::ToolIntent {
                step_id: step_id.into(),
                call: call_record.clone(),
            }],
        )
        .await?;
        if let Some((_, reason)) = self.effect_bound_status(report, control.started, control.cancel)
        {
            return Ok((not_executed(&reason), EffectStatus::NotExecuted));
        }
        let cancel = control.cancel.child_token();
        let tools = self.tools.clone();
        let resources = self.resources.clone();
        let name = call.name.clone();
        let arguments = call.arguments.clone();
        let item_id = call_record.item_id.clone();
        let worker_cancel = cancel.clone();
        let events = control.events.clone();
        let dispatch = || {
            let (start, ready) = oneshot::channel::<()>();
            let run = tokio::spawn(async move {
                let _permit = permit;
                if ready.await.is_err() {
                    return (
                        not_executed("tool dispatch was withdrawn"),
                        EffectStatus::NotExecuted,
                    );
                }
                if let Some(resources) = resources
                    && resources.contains(&name)
                {
                    return resources.execute(&name, &arguments, &worker_cancel).await;
                }
                tools
                    .execute_with_effect(
                        &name,
                        &arguments,
                        &worker_cancel,
                        &item_id,
                        events.as_ref(),
                    )
                    .await
            });
            (start, run)
        };
        let dispatched = match control.steering {
            Some(steering) => steering.fence.launch(dispatch),
            None => Some(dispatch()),
        };
        let Some((start, mut run)) = dispatched else {
            return Ok((
                not_executed("not_executed_due_to_steer"),
                EffectStatus::NotExecuted,
            ));
        };
        record(
            report,
            control.events,
            RunEvent::ToolStarted {
                id: call_record.item_id.clone(),
                name: call.name.clone(),
            },
        )
        .await;
        let _ = start.send(());
        let remaining = self
            .config
            .max_duration
            .saturating_sub(control.started.elapsed());
        let (output, expired) = tokio::select! {
            result = &mut run => (result, false),
            _ = tokio::time::sleep(remaining) => { cancel.cancel(); (run.await, true) },
        };
        let (output, effect) = output.unwrap_or_else(|error| (
            ToolResultOutput::ErrorJson {
                value: serde_json::json!({"error":format!("tool worker lost: {error}"),"worker_lost":true}),
            }, EffectStatus::Unknown,
        ));
        let effect =
            if effect != EffectStatus::NotExecuted && (control.cancel.is_cancelled() || expired) {
                EffectStatus::Unknown
            } else {
                effect
            };
        Ok((output, effect))
    }

    pub(super) async fn acquire_worker(
        &self,
        control: &BatchControl<'_>,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        let remaining = self
            .config
            .max_duration
            .saturating_sub(control.started.elapsed());
        tokio::select! {
            biased;
            _ = control.cancel.cancelled() => Err("cancelled before tool execution".into()),
            _ = async { if let Some(steering) = control.steering { steering.fence.received().await; } else { std::future::pending::<()>().await; } } => Err("not_executed_due_to_steer".into()),
            result = tokio::time::timeout(remaining, Arc::clone(&self.workers).acquire_owned()) => match result {
                Ok(Ok(permit)) => Ok(permit),
                Ok(Err(_)) => Err("tool workers unavailable".into()),
                Err(_) => Err("time bound reached waiting for a tool worker".into()),
            },
        }
    }

    pub(super) async fn execute_shared_group(
        &self,
        step_id: &str,
        invocations: Vec<Invocation>,
        report: &mut RunReport,
        control: BatchControl<'_>,
    ) -> Result<BatchOutcome, String> {
        let count = invocations.len();
        let mut pending = invocations.into_iter().enumerate().collect::<VecDeque<_>>();
        let mut running = FuturesUnordered::new();
        let mut ordered = (0..count).map(|_| None).collect::<Vec<Option<Message>>>();
        let worker_cancel = control.cancel.child_token();
        let mut outcome = BatchOutcome {
            stop: None,
            ordinary_error: false,
        };
        let mut storage_error = None;
        while !pending.is_empty() || !running.is_empty() {
            if outcome.stop.is_none() {
                outcome.stop = self.effect_bound_status(report, control.started, control.cancel);
                if outcome.stop.is_some() {
                    worker_cancel.cancel();
                }
            }
            if outcome.stop.is_none()
                && control
                    .steering
                    .as_ref()
                    .is_some_and(|value| value.fence.pending())
            {
                outcome.stop = Some((RunStatus::Failed, "not_executed_due_to_steer".into()));
                outcome.ordinary_error = true;
            }
            if running.is_empty() && (outcome.stop.is_some() || storage_error.is_some()) {
                while let Some((index, invocation)) = pending.pop_front() {
                    if storage_error.is_some() {
                        continue;
                    }
                    report.tool_calls += 1;
                    let reason = outcome
                        .stop
                        .as_ref()
                        .map_or("batch stopped", |(_, reason)| reason.as_str());
                    match settle_tool(
                        step_id,
                        report,
                        &control,
                        invocation,
                        not_executed(reason),
                        EffectStatus::NotExecuted,
                    )
                    .await
                    {
                        Ok(message) => ordered[index] = Some(message),
                        Err(error) => storage_error = Some(error),
                    }
                }
                break;
            }
            if outcome.stop.is_none()
                && storage_error.is_none()
                && running.len() < self.parallel_tools
                && let Some((_, invocation)) = pending.front()
                && let Err(error) =
                    WorkspaceTools::validate(&invocation.call.name, &invocation.call.arguments)
            {
                if let Some((index, invocation)) = pending.pop_front() {
                    report.tool_calls += 1;
                    match settle_tool(
                        step_id,
                        report,
                        &control,
                        invocation,
                        not_executed(&error),
                        EffectStatus::NotExecuted,
                    )
                    .await
                    {
                        Ok(message) => ordered[index] = Some(message),
                        Err(error) => {
                            storage_error = Some(error);
                            worker_cancel.cancel();
                        }
                    }
                    outcome.ordinary_error = true;
                    outcome.stop = Some((
                        RunStatus::Failed,
                        "remaining batch not executed after tool error or denial".into(),
                    ));
                }
                continue;
            }
            let remaining = self
                .config
                .max_duration
                .saturating_sub(control.started.elapsed());
            let completed = tokio::select! {
                biased;
                completed = running.next(), if !running.is_empty() => completed,
                _ = control.cancel.cancelled(), if outcome.stop.is_none() && storage_error.is_none() => {
                    outcome.stop = Some((RunStatus::Cancelled, "cancelled during tool execution".into()));
                    worker_cancel.cancel();
                    None
                },
                _ = tokio::time::sleep(remaining), if outcome.stop.is_none() && storage_error.is_none() => {
                    outcome.stop = Some((RunStatus::BoundExceeded, "time bound reached during tool execution".into()));
                    worker_cancel.cancel();
                    None
                },
                _ = async { if let Some(steering) = control.steering { steering.fence.received().await; } else { std::future::pending::<()>().await; } }, if outcome.stop.is_none() && storage_error.is_none() => {
                    outcome.stop = Some((RunStatus::Failed, "not_executed_due_to_steer".into()));
                    outcome.ordinary_error = true;
                    None
                },
                permit = Arc::clone(&self.workers).acquire_owned(), if !pending.is_empty() && running.len() < self.parallel_tools && outcome.stop.is_none() && storage_error.is_none() => {
                    match permit {
                        Err(_) => outcome.stop = Some((RunStatus::Failed, "tool workers unavailable".into())),
                        Ok(permit) => if let Some((index, invocation)) = pending.pop_front() {
                            report.tool_calls += 1;
                            let intent = ExecutionRecord::ToolIntent { step_id: step_id.into(), call: invocation.record.clone() };
                            if let Err(error) = commit_execution(control.commits, vec![intent]).await {
                                storage_error = Some(error);
                                worker_cancel.cancel();
                            } else {
                                let tools = self.tools.clone();
                                let cancel = worker_cancel.clone();
                                let events = control.events.clone();
                                let name = invocation.call.name.clone();
                                let arguments = invocation.call.arguments.clone();
                                let item_id = invocation.record.item_id.clone();
                                let dispatch = || {
                                    let (start, ready) = oneshot::channel::<()>();
                                    let run = tokio::spawn(async move {
                                        let _permit = permit;
                                        if ready.await.is_err() { return not_executed("tool dispatch was withdrawn"); }
                                        tools.execute(&name, &arguments, &cancel, &item_id, events.as_ref()).await
                                    });
                                    (start, run)
                                };
                                let dispatched = match control.steering {
                                    Some(steering) => steering.fence.launch(dispatch), None => Some(dispatch()),
                                };
                                if let Some((start, run)) = dispatched {
                                    record(report, control.events, RunEvent::ToolStarted {
                                        id: invocation.record.item_id.clone(), name: invocation.call.name.clone(),
                                    }).await;
                                    let _ = start.send(());
                                    running.push(async move {
                                        let output = run.await.unwrap_or_else(|error| ToolResultOutput::ErrorJson {
                                            value: serde_json::json!({"error":format!("read worker lost: {error}"),"worker_lost":true}),
                                        });
                                        (index, invocation, output)
                                    });
                                } else {
                                    match settle_tool(step_id, report, &control, invocation,
                                        not_executed("not_executed_due_to_steer"), EffectStatus::NotExecuted).await {
                                        Ok(message) => ordered[index] = Some(message),
                                        Err(error) => { storage_error = Some(error); worker_cancel.cancel(); },
                                    }
                                    outcome.stop = Some((RunStatus::Failed, "not_executed_due_to_steer".into()));
                                    outcome.ordinary_error = true;
                                }
                            }
                        },
                    }
                    None
                },
            };
            if let Some((index, invocation, output)) = completed {
                if storage_error.is_some() {
                    continue;
                }
                if output.is_error() && outcome.stop.is_none() {
                    outcome.ordinary_error = true;
                    outcome.stop = Some((
                        RunStatus::Failed,
                        "remaining batch not executed after tool error or denial".into(),
                    ));
                }
                let effect = if matches!(&output, ToolResultOutput::ErrorJson { value } if value.get("worker_lost").and_then(serde_json::Value::as_bool) == Some(true))
                {
                    report.unknown_effect = true;
                    EffectStatus::Unknown
                } else {
                    EffectStatus::Completed
                };
                match settle_tool(step_id, report, &control, invocation, output, effect).await {
                    Ok(message) => ordered[index] = Some(message),
                    Err(error) => {
                        storage_error = Some(error);
                        worker_cancel.cancel();
                    }
                }
            }
        }
        // Dropping a future waiting for spawn_blocking would orphan its worker.
        // Every started read has been awaited above, including after a store failure.
        if let Some(error) = storage_error {
            return Err(error);
        }
        for message in ordered {
            report
                .messages
                .push(message.ok_or("shared call was not settled")?);
            report.context_version = report.context_version.saturating_add(1);
        }
        if let Some(bound) = self.effect_bound_status(report, control.started, control.cancel) {
            outcome.stop = Some(bound);
            outcome.ordinary_error = false;
        }
        Ok(outcome)
    }
}
async fn settle_tool(
    step_id: &str,
    report: &mut RunReport,
    control: &BatchControl<'_>,
    invocation: Invocation,
    output: ToolResultOutput,
    effect: EffectStatus,
) -> Result<Message, String> {
    let message = Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: invocation.call.id,
            tool_name: Some(invocation.call.name.clone()),
            output: output.clone(),
            dynamic: false,
            provider_metadata: invocation.call.provider_metadata,
        }],
    };
    commit_execution(
        control.commits,
        vec![ExecutionRecord::ToolResult {
            step_id: step_id.into(),
            item_id: invocation.record.item_id.clone(),
            message: message.clone(),
            effect,
        }],
    )
    .await?;
    record(
        report,
        control.events,
        RunEvent::ToolFinished {
            id: invocation.record.item_id,
            name: invocation.call.name,
            output,
        },
    )
    .await;
    Ok(message)
}
