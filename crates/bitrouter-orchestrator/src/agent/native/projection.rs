use std::collections::BTreeSet;

use bitrouter_sdk::language_model::{Content, Message, Role, ToolResultOutput};

use crate::agent::RunEvent;
use crate::core::session::SessionSnapshot;
use crate::item::{CallOrigin, CallRecord};
use crate::store::{EffectStatus, ExecutionRecord};

#[derive(Default)]
pub(super) struct Projection {
    pub(super) records: Vec<ExecutionRecord>,
    pub(super) events: Vec<RunEvent>,
    pub(super) keys: Vec<String>,
}

pub(super) fn project(
    state: &SessionSnapshot,
    presented: &BTreeSet<String>,
    events: &[crate::core::checkpoint::DurableEvent],
) -> Projection {
    let mut projection = Projection::default();
    let Some(root) = state.agents.get(&state.agent_id) else {
        return projection;
    };
    let Some(turn) = &root.turn else {
        return projection;
    };
    for step in &turn.steps {
        let Some(plan) = &step.plan else { continue };
        let request_key = format!("request:{}", step.step_id);
        if !presented.contains(&request_key) {
            projection.keys.push(request_key);
            projection.records.push(ExecutionRecord::ModelRequest {
                step_id: step.step_id.clone(),
                item_id: step.step_id.clone(),
                context_version: root.context_revision,
                prompt: Box::new(plan.prompt.clone()),
            });
            projection.events.push(RunEvent::AssistantStarted {
                step_id: step.step_id.clone(),
                item_id: step.step_id.clone(),
            });
        }
        let response_key = format!("response:{}", step.step_id);
        if !presented.contains(&response_key)
            && (step.interrupted
                || ((step.settled && turn.status.terminal())
                    && step
                        .attempts
                        .last()
                        .and_then(|attempt| attempt.receipt.as_ref())
                        .is_none_or(|receipt| {
                            receipt.report.error.is_some()
                                || receipt.report.result.as_ref().is_some_and(|result| {
                                    !root.history.contains(&Message {
                                        role: Role::Assistant,
                                        content: result.content.clone(),
                                    })
                                })
                        })))
        {
            let observed = step
                .attempts
                .last()
                .and_then(|attempt| attempt.receipt.as_ref());
            let partial = Message {
                role: Role::Assistant,
                content: observed
                    .and_then(|receipt| receipt.report.result.as_ref())
                    .map(|result| result.content.clone())
                    .unwrap_or_default(),
            };
            let detail = observed
                .and_then(|receipt| receipt.report.error.clone())
                .or_else(|| turn.terminal_reason.clone())
                .unwrap_or_else(|| "native model attempt interrupted".into());
            projection.keys.push(response_key);
            projection.records.push(ExecutionRecord::ModelInterrupted {
                step_id: step.step_id.clone(),
                item_id: step.step_id.clone(),
                request_id: Some(plan.request_id.clone()),
                usage: observed
                    .and_then(|receipt| receipt.report.result.as_ref())
                    .and_then(|result| result.usage.clone()),
                partial: partial.clone(),
                detail: detail.clone(),
            });
            projection.events.push(RunEvent::AssistantInterrupted {
                item_id: step.step_id.clone(),
                partial,
                detail,
            });
            continue;
        }
        let Some(receipt) = step
            .attempts
            .last()
            .and_then(|attempt| attempt.receipt.as_ref())
        else {
            continue;
        };
        let Some(result) = &receipt.report.result else {
            continue;
        };
        let message = Message {
            role: Role::Assistant,
            content: result.content.clone(),
        };
        if !presented.contains(&response_key)
            && root.history.contains(&message)
            && events.iter().any(|event| {
                event.kind == "model.output.applied"
                    && event.agent_id.as_ref() == Some(&state.agent_id)
                    && event
                        .payload
                        .get("step_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(step.step_id.as_str())
                    && event.payload.get("discarded").is_none()
            })
        {
            projection.keys.push(response_key);
            let calls = message
                .content
                .iter()
                .filter_map(|part| match part {
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                        provider_executed: false,
                        ..
                    } => Some(CallRecord {
                        origin: CallOrigin::Model,
                        item_id: turn
                            .invocations
                            .iter()
                            .find(|call| {
                                call.provider_call_id == *id
                                    && call.dispatch.step_id == step.step_id
                            })
                            .map(|call| call.public_call_id.clone())
                            .or_else(|| {
                                turn.core_calls
                                    .iter()
                                    .find(|call| {
                                        call.provider_call_id == *id && call.step_id == step.step_id
                                    })
                                    .map(|call| call.public_call_id.clone())
                            })
                            .unwrap_or_else(|| id.clone()),
                        provider_call_id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            projection.records.push(ExecutionRecord::ModelResponse {
                step_id: step.step_id.clone(),
                item_id: step.step_id.clone(),
                request_id: receipt.report.request_id.clone(),
                requested_model: plan.original_model.clone(),
                usage: result.usage.clone(),
                estimated_spend_microusd: receipt
                    .report
                    .token_cost
                    .estimated_micro_usd()
                    .unwrap_or(0),
                message: message.clone(),
                calls: calls.clone(),
            });
            projection.events.push(RunEvent::ModelTurn {
                step_id: step.step_id.clone(),
                item_id: step.step_id.clone(),
                request_id: receipt.report.request_id.clone(),
                requested_model: plan.original_model.clone(),
                usage: result.usage.clone(),
            });
            projection.events.push(RunEvent::AssistantMessage {
                item_id: step.step_id.clone(),
                message,
            });
        }
    }
    for invocation in &turn.invocations {
        let intent_key = format!("intent:{}", invocation.public_call_id);
        if !presented.contains(&intent_key)
            && invocation
                .tool_observations
                .values()
                .any(|status| status.status == crate::core::protocol::ToolStatus::Running)
        {
            projection.keys.push(intent_key);
            projection.records.push(ExecutionRecord::ToolIntent {
                step_id: invocation.dispatch.step_id.clone(),
                call: CallRecord {
                    origin: CallOrigin::Model,
                    item_id: invocation.public_call_id.clone(),
                    provider_call_id: invocation.provider_call_id.clone(),
                    name: invocation.dispatch.tool.clone(),
                    arguments: invocation.dispatch.arguments.to_string(),
                },
            });
        }
        let key = format!("tool:{}", invocation.public_call_id);
        let Some(result) = &invocation.result else {
            continue;
        };
        if presented.contains(&key) {
            continue;
        }
        let output =
            serde_json::from_str::<ToolResultOutput>(&result.output).unwrap_or_else(|_| {
                if result.status == crate::core::protocol::ToolOutcome::Succeeded {
                    ToolResultOutput::Text {
                        value: result.output.clone(),
                    }
                } else {
                    ToolResultOutput::ErrorText {
                        value: format!("{:?}: {}", result.status, result.output),
                    }
                }
            });
        projection.keys.push(key);
        projection.records.push(ExecutionRecord::ToolResult {
            step_id: invocation.dispatch.step_id.clone(),
            item_id: invocation.public_call_id.clone(),
            message: Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: invocation.provider_call_id.clone(),
                    tool_name: Some(invocation.dispatch.tool.clone()),
                    dynamic: false,
                    output: output.clone(),
                    provider_metadata: crate::context::tool_result_metadata(
                        &root.history,
                        &invocation.provider_call_id,
                    ),
                }],
            },
            effect: match result.status {
                crate::core::protocol::ToolOutcome::EffectUnknown => EffectStatus::Unknown,
                crate::core::protocol::ToolOutcome::NotExecuted
                | crate::core::protocol::ToolOutcome::Denied => EffectStatus::NotExecuted,
                _ => EffectStatus::Completed,
            },
        });
        projection.events.push(RunEvent::ToolFinished {
            id: invocation.public_call_id.clone(),
            name: invocation.dispatch.tool.clone(),
            output,
        });
    }
    for call in &turn.core_calls {
        let Some(result) = &call.result else { continue };
        if !call.consumed {
            continue;
        }
        let key = format!("tool:{}", call.public_call_id);
        if presented.contains(&key) {
            continue;
        }
        let Some((tool_name, output)) = root.history.iter().rev().flat_map(|message| &message.content).find_map(|part| match part {
            Content::ToolResult { call_id, tool_name, output, .. } if call_id == &call.provider_call_id && matches!(output, ToolResultOutput::Text { value } if serde_json::from_str::<serde_json::Value>(value).ok().as_ref() == Some(result)) => Some((tool_name.clone(), output.clone())),
            _ => None,
        }) else { continue };
        projection.keys.push(key);
        projection.records.push(ExecutionRecord::ToolResult {
            step_id: call.step_id.clone(),
            item_id: call.public_call_id.clone(),
            message: Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: call.provider_call_id.clone(),
                    tool_name: tool_name.clone(),
                    output: output.clone(),
                    dynamic: false,
                    provider_metadata: Default::default(),
                }],
            },
            effect: EffectStatus::Completed,
        });
        projection.events.push(RunEvent::ToolFinished {
            id: call.public_call_id.clone(),
            name: tool_name.unwrap_or_default(),
            output,
        });
    }
    projection
}
