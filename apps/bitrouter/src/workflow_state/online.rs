use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::Prompt;

use crate::policy_table_router::PolicyTable;
use crate::workflow_state::extractors::{
    ExtractorInput, extract_workflow_state, parse_compatibility_harness,
};
use crate::workflow_state::ir::{HarnessId, ProtocolKind, WorkflowStateIR};
use crate::workflow_state::predictive::{
    PredictiveRouteIR, PredictiveRouteProjection, predict_next_step,
};
use crate::workflow_state::session::{WorkflowIdentityTracker, resolve_workflow_identity};

pub struct OnlineWorkflowState {
    pub ir: WorkflowStateIR,
    pub predictive: PredictiveRouteIR,
    legacy_fingerprint: String,
    routing_key: String,
    baseline_routing_key: String,
    observed_routing_key: String,
}

impl OnlineWorkflowState {
    pub fn from_headers(headers: &HeaderMap, prompt: &Prompt) -> Self {
        Self::for_named_policy(headers, prompt)
    }

    /// Build the named-policy projection without sharing process-local identity
    /// tracker state across requests. Adapter identity remains available on the
    /// returned IR for diagnostics, but cannot become causal routing state.
    pub fn for_named_policy(headers: &HeaderMap, prompt: &Prompt) -> Self {
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
        let mut ir = extract_workflow_state(&input);
        ir.identity = resolve_workflow_identity(&input, tracker);
        let legacy_fingerprint = PolicyTable::fingerprint(prompt);
        let predictive = predict_next_step(&ir, prompt);
        let predictive_projection = PredictiveRouteProjection::new(
            predictive.task_family,
            predictive.next_step_role,
            predictive.route_risk,
        );
        let routing_key = predictive_projection.key();
        let baseline_routing_key = predictive_projection.unknown_baseline().key();
        let observed_routing_key = ir.route_projection().key();
        Self {
            ir,
            predictive,
            legacy_fingerprint,
            routing_key,
            baseline_routing_key,
            observed_routing_key,
        }
    }

    pub fn routing_key(&self) -> &str {
        &self.routing_key
    }

    pub fn observed_routing_key(&self) -> &str {
        &self.observed_routing_key
    }

    pub fn baseline_routing_key(&self) -> &str {
        &self.baseline_routing_key
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
        Content, GenerationParams, Message, Prompt, ProviderMetadata, Role, ToolResultOutput,
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

    fn prompt_with_unrecognized_json_action() -> Prompt {
        Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![
                Message::text(Role::User, "continue"),
                Message::text(
                    Role::Assistant,
                    r#"{"commands":[{"keystrokes":"cargo test -p bitrouter"}]}"#,
                ),
            ],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn prompt_after_completed_read() -> Prompt {
        let mut prompt = prompt_after_tool("read_file");
        prompt.messages.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call_read_file".to_string(),
                tool_name: None,
                output: ToolResultOutput::Text {
                    value: "source contents".to_string(),
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        });
        prompt
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
        assert_eq!(state.routing_key(), "agent_route/v1|unknown|unknown|normal");
        assert_eq!(
            state.observed_routing_key(),
            "agent_trace/v2|tool_followup|normal"
        );
        assert_eq!(state.baseline_routing_key(), state.routing_key());
        assert_eq!(state.ir.last_tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn online_state_separates_predictive_and_observed_route_keys() {
        let prompt = prompt_after_completed_read();
        let state = OnlineWorkflowState::from_prompt(
            &HeaderMap::new(),
            &prompt,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        );

        assert_eq!(
            state.routing_key(),
            "agent_route/v1|unknown|implement|normal"
        );
        assert_eq!(
            state.observed_routing_key(),
            "agent_trace/v2|tool_followup|normal"
        );
        assert_eq!(state.baseline_routing_key(), state.routing_key());
    }

    #[test]
    fn task_aware_online_state_uses_one_v1_projection_and_unknown_baseline() {
        let prompt = Prompt {
            model: "inbound".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message::text(
                Role::User,
                "Fix the parser panic in src/parser.rs after this regression failed.",
            )],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let state = OnlineWorkflowState::from_headers(&HeaderMap::new(), &prompt);

        assert_eq!(
            state.routing_key(),
            "agent_route/v1|code:debugging|implement|normal"
        );
        assert_eq!(
            state.baseline_routing_key(),
            "agent_route/v1|unknown|implement|normal"
        );

        let mut private_headers = HeaderMap::new();
        private_headers.insert("x-bitrouter-harness", HeaderValue::from_static("smithers"));
        private_headers.insert(
            "x-smithers-workflow-id",
            HeaderValue::from_static("private-release-workflow"),
        );
        private_headers.insert(
            "x-smithers-node-id",
            HeaderValue::from_static("generated-debug-task"),
        );
        let private_state = OnlineWorkflowState::from_headers(&private_headers, &prompt);

        assert_eq!(
            private_state.predictive.task_family,
            state.predictive.task_family
        );
        assert_eq!(
            private_state.predictive.task_family_evidence,
            state.predictive.task_family_evidence
        );
    }

    #[test]
    fn online_state_keeps_legacy_headers_as_diagnostic_evidence() {
        let prompt = prompt_after_tool("exec_command");
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", "codex".parse().unwrap());
        headers.insert("x-bitrouter-protocol", "responses".parse().unwrap());

        let state = OnlineWorkflowState::from_headers(&headers, &prompt);

        assert_eq!(state.ir.harness_id, HarnessId::Generic);
        assert_eq!(state.ir.protocol, ProtocolKind::Responses);
        assert_eq!(state.routing_key(), "agent_route/v1|unknown|unknown|normal");
        assert_eq!(
            state.observed_routing_key(),
            "agent_trace/v2|tool_followup|normal"
        );
        assert!(state.ir.evidence.iter().any(|e| {
            e.kind == "trace_adapter" && e.value == "compatibility_harness_hint:Codex"
        }));
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
        assert_eq!(state.routing_key(), "agent_route/v1|unknown|unknown|normal");
        assert_eq!(
            state.observed_routing_key(),
            "agent_trace/v2|tool_followup|normal"
        );
        assert!(
            state.ir.legacy_routing_key().starts_with(
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
    fn compatibility_harness_headers_cannot_select_runtime_parsers() {
        let prompt = prompt_with_unrecognized_json_action();
        let baseline = OnlineWorkflowState::from_headers(&HeaderMap::new(), &prompt);
        assert_eq!(
            baseline.ir.state_kind,
            crate::workflow_state::ir::WorkflowStateKind::Planning
        );

        for harness in [
            "generic",
            "hermes",
            "claude_code",
            "codex",
            "smithers",
            "terminus_2",
            "openclaw",
            "unknown",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-bitrouter-harness", harness.parse().unwrap());
            let state = OnlineWorkflowState::from_headers(&headers, &prompt);

            assert_eq!(state.ir.state_kind, baseline.ir.state_kind, "{harness}");
            assert_eq!(state.routing_key(), baseline.routing_key(), "{harness}");
            assert_eq!(state.ir.protocol, baseline.ir.protocol, "{harness}");
            assert_eq!(state.ir.session, baseline.ir.session, "{harness}");
        }
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
