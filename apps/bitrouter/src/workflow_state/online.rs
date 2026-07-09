use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::Prompt;

use crate::policy_table_router::PolicyTable;
use crate::workflow_state::extractors::{ExtractorInput, extract_workflow_state};
use crate::workflow_state::ir::{HarnessId, ProtocolKind, WorkflowStateIR};

pub struct OnlineWorkflowState {
    pub ir: WorkflowStateIR,
    legacy_fingerprint: String,
    routing_key: String,
}

impl OnlineWorkflowState {
    pub fn from_headers(headers: &HeaderMap, prompt: &Prompt) -> Self {
        let (harness_hint, protocol_hint) = infer_online_context(headers);
        Self::from_prompt(headers, prompt, harness_hint, protocol_hint)
    }

    pub fn from_prompt(
        headers: &HeaderMap,
        prompt: &Prompt,
        harness_hint: Option<HarnessId>,
        protocol_hint: ProtocolKind,
    ) -> Self {
        let raw_body = serde_json::Value::Null;
        let ir = extract_workflow_state(&ExtractorInput {
            harness_hint,
            protocol_hint,
            headers,
            raw_body: &raw_body,
            prompt,
        });
        let legacy_fingerprint = PolicyTable::fingerprint(prompt);
        let routing_key = ir.routing_key();
        Self {
            ir,
            legacy_fingerprint,
            routing_key,
        }
    }

    pub fn routing_key(&self) -> &str {
        &self.routing_key
    }

    pub fn legacy_fingerprint(&self) -> &str {
        &self.legacy_fingerprint
    }
}

fn infer_online_context(headers: &HeaderMap) -> (Option<HarnessId>, ProtocolKind) {
    let explicit_harness = header_value(headers, "x-bitrouter-harness").and_then(parse_harness);
    let explicit_protocol = header_value(headers, "x-bitrouter-protocol")
        .or_else(|| header_value(headers, "x-bitrouter-inbound-protocol"))
        .and_then(parse_protocol);
    if explicit_harness.is_some() || explicit_protocol.is_some() {
        return (
            explicit_harness,
            explicit_protocol.unwrap_or(ProtocolKind::Unknown),
        );
    }

    if headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|beta| beta.trim().starts_with("claude-code"))
        })
    {
        return (Some(HarnessId::ClaudeCode), ProtocolKind::Messages);
    }

    (None, ProtocolKind::Unknown)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_harness(value: &str) -> Option<HarnessId> {
    match value.trim().to_ascii_lowercase().as_str() {
        "generic" => Some(HarnessId::Generic),
        "hermes" => Some(HarnessId::Hermes),
        "claude" | "claude_code" | "claude-code" => Some(HarnessId::ClaudeCode),
        "codex" => Some(HarnessId::Codex),
        "openclaw" | "open_claw" | "open-claw" => Some(HarnessId::OpenClaw),
        "unknown" => Some(HarnessId::Unknown),
        _ => None,
    }
}

fn parse_protocol(value: &str) -> Option<ProtocolKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat_completions" | "chat-completions" | "chat" => Some(ProtocolKind::ChatCompletions),
        "messages" | "anthropic_messages" | "anthropic-messages" => Some(ProtocolKind::Messages),
        "responses" | "openai_responses" | "openai-responses" => Some(ProtocolKind::Responses),
        "openclaw_runtime" | "openclaw-runtime" => Some(ProtocolKind::OpenClawRuntime),
        "unknown" => Some(ProtocolKind::Unknown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role,
    };

    use crate::workflow_state::ir::{HarnessId, ProtocolKind};
    use crate::workflow_state::online::OnlineWorkflowState;

    fn prompt_after_tool(tool: &str) -> Prompt {
        Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![
                Message::text(Role::User, "run the tool"),
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
                },
            ],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn online_state_exposes_ir_key_and_legacy_fingerprint() {
        let prompt = prompt_after_tool("Bash");
        let state = OnlineWorkflowState::from_prompt(
            &HeaderMap::new(),
            &prompt,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        );

        assert_eq!(state.legacy_fingerprint(), "after_Bash");
        assert!(state.routing_key().contains("tool_followup"));
        assert_eq!(state.ir.last_tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn online_state_uses_explicit_harness_and_protocol_headers() {
        let prompt = prompt_after_tool("exec_command");
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", "codex".parse().unwrap());
        headers.insert("x-bitrouter-protocol", "responses".parse().unwrap());

        let state = OnlineWorkflowState::from_headers(&headers, &prompt);

        assert_eq!(state.ir.harness_id, HarnessId::Codex);
        assert_eq!(state.ir.protocol, ProtocolKind::Responses);
        assert!(
            state
                .routing_key()
                .starts_with("codex|responses|tool_followup")
        );
    }
}
