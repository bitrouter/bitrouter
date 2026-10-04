//! Canonical tool/history pairing, shared by execution and capacity projection.

use super::*;
use bitrouter_sdk::language_model::types::ToolResultOutput;

pub(super) fn consume(agent: &mut AgentState) -> Result<(), CoreError> {
    let turn = agent
        .turn
        .as_mut()
        .ok_or_else(|| reject(ErrorCode::Busy, "agent has no turn"))?;
    let mut messages = Vec::new();
    let mut context_sources = Vec::new();
    let mut verified = None;
    for call in turn.invocations.iter_mut().filter(|call| !call.consumed) {
        let result = call
            .result
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "tool batch is not complete"))?;
        if result.status == ToolOutcome::EffectUnknown {
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "tool effect is unknown",
            ));
        }
        if matches!(result.status, ToolOutcome::Succeeded | ToolOutcome::Failed) {
            context_sources.push(ContextSource {
                permission_revision: call.dispatch.permission_revision,
                workspace_revision: result.workspace_revision.clone(),
                tool_manifest_digest: call.dispatch.tool_manifest_digest.clone(),
                materials: Vec::new(),
            });
        }
        if call.dispatch.verification {
            verified = Some(result.status == ToolOutcome::Succeeded);
        } else {
            let output = if result.status == ToolOutcome::Succeeded {
                ToolResultOutput::Text {
                    value: result.output.clone(),
                }
            } else {
                ToolResultOutput::ErrorText {
                    value: format!("{:?}: {}", result.status, result.output),
                }
            };
            messages.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult {
                    call_id: call.provider_call_id.clone(),
                    tool_name: Some(call.dispatch.tool.clone()),
                    dynamic: false,
                    output,
                    provider_metadata: Default::default(),
                }],
            });
        }
        call.consumed = true;
    }
    for call in turn.core_calls.iter_mut().filter(|call| !call.consumed) {
        let result = call
            .result
            .as_ref()
            .ok_or_else(|| reject(ErrorCode::Busy, "collaboration batch is not complete"))?;
        if matches!(call.action, Action::Wait { .. })
            && let Some(observations) = result["value"]["agents"].as_array()
        {
            for observation in observations {
                let sources: Vec<ContextSource> =
                    serde_json::from_value(observation["context_sources"].clone())
                        .map_err(json_error)?;
                context_sources.extend(sources);
            }
        }
        messages.push(core_message(call, result)?);
        call.consumed = true;
    }
    if turn.status != AgentStatus::Cancelling {
        turn.status = AgentStatus::Runnable;
    }
    if let Some(success) = verified {
        turn.terminal_reason = Some(format!("harness verification succeeded: {success}"));
    }
    agent.history.extend(messages);
    for source in context_sources {
        if !agent.context_sources.contains(&source) {
            agent.context_sources.push(source);
        }
    }
    agent.context_revision = agent
        .context_revision
        .checked_add(1)
        .ok_or_else(|| reject(ErrorCode::LimitExceeded, "context revision exhausted"))?;
    Ok(())
}

pub(super) fn core_message(call: &Call, result: &Value) -> Result<Message, CoreError> {
    Ok(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: call.provider_call_id.clone(),
            tool_name: Some(call.action.name().into()),
            dynamic: false,
            output: ToolResultOutput::Text {
                value: serde_json::to_string(result).map_err(json_error)?,
            },
            provider_metadata: Default::default(),
        }],
    })
}
