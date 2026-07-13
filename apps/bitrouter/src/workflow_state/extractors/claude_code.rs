use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, ProtocolKind, SessionConfidence, WorkflowStateIR,
};

pub struct ClaudeCodeExtractor;

impl WorkflowStateExtractor for ClaudeCodeExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: Some(HarnessId::ClaudeCode),
            protocol_hint: ProtocolKind::Messages,
            headers: input.headers,
            raw_body: input.raw_body,
            prompt: input.prompt,
        });
        if headers_indicate_claude_code(input.headers) {
            ir.evidence.push(Evidence {
                kind: "claude_code_beta".to_string(),
                value: "anthropic-beta contains claude-code".to_string(),
                confidence: 0.95,
                level: EvidenceLevel::Observed,
            });
        }
        if ir.session.key.is_none()
            && let Some(user_id) = input
                .raw_body
                .get("metadata")
                .and_then(|m| m.get("user_id"))
                .and_then(|v| v.as_str())
        {
            ir.session.key = Some(user_id.to_string());
            ir.session.confidence = SessionConfidence::Low;
            ir.session.source = Some("raw_body.metadata.user_id".to_string());
        }
        ir
    }
}

fn headers_indicate_claude_code(headers: &bitrouter_sdk::HeaderMap) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|beta| beta.trim().starts_with("claude-code"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        GenerationParams, Message, Prompt, ProviderMetadata, Role,
    };
    use http::HeaderValue;

    use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
    use crate::workflow_state::ir::{HarnessId, ProtocolKind};

    fn prompt() -> Prompt {
        Prompt {
            model: "claude-sonnet-4-6".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message::text(Role::User, "inspect")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn claude_code_adapter_detects_agent_profile_beta() {
        let prompt = prompt();
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,tools-2024-05-16"),
        );
        let raw_body = serde_json::json!({});
        let ir = ClaudeCodeExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::Messages,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::ClaudeCode);
        assert!(ir.evidence.iter().any(|e| e.kind == "claude_code_beta"));
    }
}
