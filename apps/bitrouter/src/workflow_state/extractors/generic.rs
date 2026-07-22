use bitrouter_sdk::language_model::types::{
    Content, Message, Prompt, Role, ToolResultContentPart, ToolResultOutput,
};

use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    CapabilityConstraints, ContextSizeBucket, Evidence, EvidenceLevel, HarnessId, RecoverySignal,
    RequirementLevel, ToolDensity, WorkflowStateIR, WorkflowStateKind,
};
use crate::workflow_state::session::resolve_session_signal;

pub struct GenericPromptExtractor;

impl WorkflowStateExtractor for GenericPromptExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let prompt = input.prompt;
        let (mut state_kind, last_tool_name, state_evidence) = classify_state(prompt);
        let context_size = context_size_bucket(prompt);
        let tool_density = tool_density(prompt);
        let recovery_signal = recovery_signal(prompt);
        if recovery_signal == RecoverySignal::LikelyRecovery
            && matches!(state_kind, WorkflowStateKind::ToolFollowup)
        {
            state_kind = WorkflowStateKind::Recovery;
        }
        let mut capability_constraints = CapabilityConstraints {
            tool_reliability: if tool_density == ToolDensity::None {
                RequirementLevel::Low
            } else {
                RequirementLevel::High
            },
            code_reasoning: if matches!(
                state_kind,
                WorkflowStateKind::Debug | WorkflowStateKind::Review | WorkflowStateKind::Recovery
            ) || recovery_signal == RecoverySignal::LikelyRecovery
            {
                RequirementLevel::Medium
            } else {
                RequirementLevel::Low
            },
            context_pressure: match context_size {
                ContextSizeBucket::Large => RequirementLevel::High,
                ContextSizeBucket::Medium => RequirementLevel::Medium,
                ContextSizeBucket::Small => RequirementLevel::Low,
                ContextSizeBucket::Unknown => RequirementLevel::Unknown,
            },
            latency_sensitivity: RequirementLevel::Low,
            expected_redo_penalty: if recovery_signal == RecoverySignal::LikelyRecovery {
                RequirementLevel::High
            } else {
                RequirementLevel::Medium
            },
            output_precision: RequirementLevel::Medium,
            compatibility: Vec::new(),
        };
        if tool_density != ToolDensity::None {
            capability_constraints
                .compatibility
                .push("requires_structured_tools".to_string());
        }

        let resolved_session = resolve_session_signal(input);
        let mut evidence = vec![state_evidence];
        evidence.extend(resolved_session.evidence);
        evidence.push(Evidence {
            kind: "context_size".to_string(),
            value: format!("{context_size:?}"),
            confidence: 0.65,
            level: EvidenceLevel::Inferred,
        });
        if recovery_signal == RecoverySignal::LikelyRecovery {
            evidence.push(Evidence {
                kind: "recovery_marker".to_string(),
                value: "recent content contains error marker".to_string(),
                confidence: 0.75,
                level: EvidenceLevel::Inferred,
            });
        }

        WorkflowStateIR {
            harness_id: input.harness_hint.clone().unwrap_or(HarnessId::Generic),
            protocol: input.protocol_hint.clone(),
            state_kind,
            active_workflow: None,
            subagent_role: None,
            last_tool_name,
            tool_density,
            context_size,
            recovery_signal,
            capability_constraints,
            session: resolved_session.signal,
            identity: Default::default(),
            confidence: 0.7,
            evidence,
        }
    }
}

fn classify_state(prompt: &Prompt) -> (WorkflowStateKind, Option<String>, Evidence) {
    for message in prompt.messages.iter().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        if let Some(name) = message.content.iter().rev().find_map(tool_call_name) {
            return (
                WorkflowStateKind::ToolFollowup,
                Some(name.to_string()),
                Evidence {
                    kind: "last_assistant_tool_call".to_string(),
                    value: name.to_string(),
                    confidence: 0.9,
                    level: EvidenceLevel::Observed,
                },
            );
        }
        return (
            WorkflowStateKind::Planning,
            None,
            Evidence {
                kind: "last_assistant_text_turn".to_string(),
                value: "assistant turn without tool call".to_string(),
                confidence: 0.55,
                level: EvidenceLevel::Inferred,
            },
        );
    }
    (
        WorkflowStateKind::Opening,
        None,
        Evidence {
            kind: "no_assistant_turn".to_string(),
            value: "opening".to_string(),
            confidence: 0.95,
            level: EvidenceLevel::Observed,
        },
    )
}

fn tool_call_name(content: &Content) -> Option<&str> {
    match content {
        Content::ToolCall { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

fn tool_density(prompt: &Prompt) -> ToolDensity {
    let declared_tools = prompt.tools.len();
    let tool_events = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|c| matches!(c, Content::ToolCall { .. } | Content::ToolResult { .. }))
        .count();
    match declared_tools + tool_events {
        0 => ToolDensity::None,
        1..=2 => ToolDensity::Low,
        _ => ToolDensity::High,
    }
}

fn context_size_bucket(prompt: &Prompt) -> ContextSizeBucket {
    let size = prompt
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .map(content_size)
        .sum::<usize>()
        + prompt.system.as_ref().map_or(0, |s| s.len());
    match size {
        0..=10_000 => ContextSizeBucket::Small,
        10_001..=50_000 => ContextSizeBucket::Medium,
        _ => ContextSizeBucket::Large,
    }
}

fn content_size(content: &Content) -> usize {
    match content {
        Content::Text { text, .. } | Content::Reasoning { text, .. } => text.len(),
        Content::ToolCall {
            name, arguments, ..
        } => name.len() + arguments.len(),
        Content::ToolResult { output, .. } => tool_result_text(output).len(),
        other => serde_json::to_string(other).map_or(0, |s| s.len()),
    }
}

fn recovery_signal(prompt: &Prompt) -> RecoverySignal {
    let recent_text = prompt
        .messages
        .iter()
        .rev()
        .take(4)
        .flat_map(message_text)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let markers = [
        "error:",
        "failed",
        "traceback",
        "exit code 1",
        "command not found",
        "no such file or directory",
        "nonzero",
        "panic",
        "exception",
    ];
    if markers.iter().any(|marker| recent_text.contains(marker)) {
        RecoverySignal::LikelyRecovery
    } else {
        RecoverySignal::None
    }
}

fn message_text(message: &Message) -> Vec<String> {
    message.content.iter().filter_map(content_text).collect()
}

fn content_text(content: &Content) -> Option<String> {
    match content {
        Content::Text { text, .. } | Content::Reasoning { text, .. } => Some(text.clone()),
        Content::ToolCall {
            name, arguments, ..
        } => Some(format!("{name} {arguments}")),
        Content::ToolResult { output, .. } => Some(tool_result_text(output)),
        _ => None,
    }
}

fn tool_result_text(output: &ToolResultOutput) -> String {
    match output {
        ToolResultOutput::Text { value }
        | ToolResultOutput::ErrorText { value }
        | ToolResultOutput::ExecutionDenied {
            reason: Some(value),
        } => value.clone(),
        ToolResultOutput::ExecutionDenied { reason: None } => String::new(),
        ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
            value.to_string()
        }
        ToolResultOutput::Content { value } => value.iter().filter_map(tool_part_text).collect(),
    }
}

fn tool_part_text(part: &ToolResultContentPart) -> Option<String> {
    match part {
        ToolResultContentPart::Text { text } => Some(text.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role, Tool, ToolResultOutput,
    };

    use crate::workflow_state::ir::{
        ContextSizeBucket, ProtocolKind, RecoverySignal, ToolDensity, WorkflowStateKind,
    };

    fn prompt(messages: Vec<Message>, tools: Vec<Tool>) -> Prompt {
        Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools,
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn assistant_calls(tool: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: format!("call_{tool}"),
                name: tool.to_string(),
                arguments: "{}".to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn tool_result(text: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call_bash".to_string(),
                tool_name: Some("bash".to_string()),
                output: ToolResultOutput::Text {
                    value: text.to_string(),
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    fn extract(prompt: &Prompt) -> crate::workflow_state::ir::WorkflowStateIR {
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({});
        GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt,
        })
    }

    #[test]
    fn generic_extracts_opening_from_no_assistant_turn() {
        let prompt = prompt(vec![user("start")], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.state_kind, WorkflowStateKind::Opening);
        assert_eq!(ir.tool_density, ToolDensity::None);
    }

    #[test]
    fn generic_extracts_tool_followup_from_last_tool_call() {
        let prompt = prompt(vec![user("run tests"), assistant_calls("bash")], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.state_kind, WorkflowStateKind::ToolFollowup);
        assert_eq!(ir.last_tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn generic_marks_recovery_when_recent_tool_result_contains_error() {
        let prompt = prompt(
            vec![
                user("run tests"),
                assistant_calls("bash"),
                tool_result("error: test failed with exit code 1"),
            ],
            Vec::new(),
        );
        let ir = extract(&prompt);
        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
    }

    #[test]
    fn generic_splits_command_not_found_tool_followup_into_recovery_state() {
        let prompt = prompt(
            vec![
                user("verify the regex"),
                assistant_calls("bash"),
                tool_result("/bin/bash: line 1: python3: command not found"),
            ],
            Vec::new(),
        );
        let ir = extract(&prompt);
        assert_eq!(ir.recovery_signal, RecoverySignal::LikelyRecovery);
        assert_eq!(ir.state_kind, WorkflowStateKind::Recovery);
        assert_eq!(ir.last_tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn generic_buckets_context_size() {
        let large = "x".repeat(80_000);
        let prompt = prompt(vec![user(&large)], Vec::new());
        let ir = extract(&prompt);
        assert_eq!(ir.context_size, ContextSizeBucket::Large);
    }
}
