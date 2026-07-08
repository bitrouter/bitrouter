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
}
