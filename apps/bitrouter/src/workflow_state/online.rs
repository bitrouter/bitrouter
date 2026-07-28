use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::Prompt;

use crate::policy_table_router::PolicyTable;
use crate::workflow_state::extractors::{
    ExtractorInput, adapter_protocol_hint, detect_trace_adapter, extract_workflow_state,
    parse_compatibility_harness,
};
use crate::workflow_state::ir::{HarnessId, ProtocolKind, WorkflowStateIR};
use crate::workflow_state::session::{WorkflowIdentityTracker, resolve_workflow_identity};

pub struct OnlineWorkflowState {
    pub ir: WorkflowStateIR,
    legacy_fingerprint: String,
    routing_key: String,
    legacy_routing_key: String,
}

impl OnlineWorkflowState {
    pub fn from_headers(headers: &HeaderMap, prompt: &Prompt) -> Self {
        let tracker = WorkflowIdentityTracker::default();
        Self::from_headers_with_tracker(headers, prompt, &tracker)
    }

    pub fn from_headers_with_tracker(
        headers: &HeaderMap,
        prompt: &Prompt,
        tracker: &WorkflowIdentityTracker,
    ) -> Self {
        let (harness_hint, protocol_hint) = infer_online_context(headers, prompt);
        Self::from_prompt_with_tracker(headers, prompt, harness_hint, protocol_hint, tracker)
    }

    pub fn from_prompt(
        headers: &HeaderMap,
        prompt: &Prompt,
        harness_hint: Option<HarnessId>,
        protocol_hint: ProtocolKind,
    ) -> Self {
        let tracker = WorkflowIdentityTracker::default();
        Self::from_prompt_with_tracker(headers, prompt, harness_hint, protocol_hint, &tracker)
    }

    pub fn from_prompt_with_tracker(
        headers: &HeaderMap,
        prompt: &Prompt,
        harness_hint: Option<HarnessId>,
        protocol_hint: ProtocolKind,
        tracker: &WorkflowIdentityTracker,
    ) -> Self {
        let raw_body = serde_json::Value::Object(
            prompt
                .params
                .extra
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        let input = ExtractorInput {
            harness_hint,
            protocol_hint,
            headers,
            raw_body: &raw_body,
            prompt,
        };
        let adapter = detect_trace_adapter(&input);
        let selected_input = ExtractorInput {
            harness_hint: Some(adapter.source.clone()),
            protocol_hint: adapter_protocol_hint(&input, &adapter),
            headers,
            raw_body: &raw_body,
            prompt,
        };
        let mut ir = extract_workflow_state(&selected_input);
        ir.identity = resolve_workflow_identity(&selected_input, tracker);
        let legacy_fingerprint = PolicyTable::fingerprint(prompt);
        let routing_key = ir.route_projection().key();
        let legacy_routing_key = ir.legacy_routing_key();
        Self {
            ir,
            legacy_fingerprint,
            routing_key,
            legacy_routing_key,
        }
    }

    pub fn routing_key(&self) -> &str {
        &self.routing_key
    }

    pub fn legacy_routing_key(&self) -> &str {
        &self.legacy_routing_key
    }

    pub fn legacy_fingerprint(&self) -> &str {
        &self.legacy_fingerprint
    }
}

fn infer_online_context(
    headers: &HeaderMap,
    _prompt: &Prompt,
) -> (Option<HarnessId>, ProtocolKind) {
    let explicit_harness =
        header_value(headers, "x-bitrouter-harness").and_then(parse_compatibility_harness);
    let explicit_protocol = header_value(headers, "x-bitrouter-inbound-protocol")
        .or_else(|| header_value(headers, "x-bitrouter-protocol"))
        .and_then(parse_protocol);
    let protocol_hint = explicit_protocol.unwrap_or(ProtocolKind::Unknown);
    (explicit_harness, protocol_hint)
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
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
    use http::HeaderValue;

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
        assert_eq!(state.routing_key(), "agent_trace/v1|tool_followup|normal");
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
        assert_eq!(state.routing_key(), "agent_trace/v1|tool_followup|normal");
    }

    #[test]
    fn smithers_headers_remain_available_only_in_the_legacy_routing_key() {
        let prompt = prompt_after_tool("analyze");
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", "smithers".parse().unwrap());
        headers.insert("x-bitrouter-protocol", "chat".parse().unwrap());
        headers.insert("x-smithers-workflow-id", "release-review".parse().unwrap());
        headers.insert("x-smithers-node-id", "analyze-risk".parse().unwrap());
        headers.insert("x-bitrouter-workflow-session", "run-1".parse().unwrap());

        let state = OnlineWorkflowState::from_headers(&headers, &prompt);

        assert_eq!(state.ir.harness_id, HarnessId::Smithers);
        assert_eq!(state.ir.active_workflow.as_deref(), Some("release-review"));
        assert_eq!(state.ir.subagent_role.as_deref(), Some("analyze-risk"));
        assert_eq!(state.routing_key(), "agent_trace/v1|tool_followup|normal");
        assert!(
            state.legacy_routing_key().starts_with(
                "smithers|chat_completions|tool_followup|release-review|analyze-risk|"
            )
        );
    }

    #[test]
    fn superpowers_headers_do_not_change_online_route_key() {
        let prompt = prompt_after_tool("apply_patch");
        let mut baseline_headers = HeaderMap::new();
        baseline_headers.insert("x-bitrouter-harness", HeaderValue::from_static("codex"));
        baseline_headers.insert(
            "x-bitrouter-protocol",
            HeaderValue::from_static("responses"),
        );
        let baseline = OnlineWorkflowState::from_headers(&baseline_headers, &prompt);
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", HeaderValue::from_static("codex"));
        headers.insert(
            "x-bitrouter-protocol",
            HeaderValue::from_static("responses"),
        );
        headers.insert("x-superpowers-phase", HeaderValue::from_static("unknown"));
        headers.insert(
            "x-superpowers-skill",
            HeaderValue::from_static("superpowers:subagent-driven-development"),
        );
        headers.insert(
            "x-bitrouter-agent-role",
            HeaderValue::from_static("implementer"),
        );
        headers.insert(
            "x-bitrouter-task-complexity",
            HeaderValue::from_static("mechanical"),
        );

        let state = OnlineWorkflowState::from_headers(&headers, &prompt);

        assert_eq!(state.ir.state_kind, baseline.ir.state_kind);
        assert_eq!(state.routing_key(), baseline.routing_key());
    }

    #[test]
    fn smithers_blank_identity_headers_are_ignored() {
        let prompt = prompt_after_tool("analyze");
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", "smithers".parse().unwrap());
        headers.insert("x-bitrouter-protocol", "chat".parse().unwrap());
        headers.insert("x-smithers-workflow-id", "   ".parse().unwrap());
        headers.insert("x-smithers-node-id", "\t".parse().unwrap());

        let state = OnlineWorkflowState::from_headers(&headers, &prompt);

        assert_eq!(state.ir.active_workflow, None);
        assert_eq!(state.ir.subagent_role, None);
    }

    #[test]
    fn smithers_harness_serializes_stably() {
        assert_eq!(
            serde_json::to_string(&HarnessId::Smithers).unwrap(),
            "\"smithers\""
        );
    }
}
