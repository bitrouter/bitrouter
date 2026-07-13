use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, ProtocolKind, SessionConfidence, WorkflowStateIR,
};

pub struct CodexResponsesExtractor;

impl WorkflowStateExtractor for CodexResponsesExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: Some(HarnessId::Codex),
            protocol_hint: ProtocolKind::Responses,
            headers: input.headers,
            raw_body: input.raw_body,
            prompt: input.prompt,
        });
        if let Some(previous_response_id) = input
            .raw_body
            .get("previous_response_id")
            .and_then(|v| v.as_str())
        {
            if ir.session.key.is_none() {
                ir.session.key = Some(previous_response_id.to_string());
                ir.session.confidence = SessionConfidence::Medium;
                ir.session.source = Some("raw_body.previous_response_id".to_string());
            }
            ir.evidence.push(Evidence {
                kind: "server_side_context_gap".to_string(),
                value: "previous_response_id means prior turns may be hidden".to_string(),
                confidence: 0.95,
                level: EvidenceLevel::Missing,
            });
            ir.confidence = ir.confidence.min(0.65);
        }
        ir
    }
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
    use crate::workflow_state::ir::{EvidenceLevel, HarnessId, ProtocolKind, SessionConfidence};

    fn prompt() -> Prompt {
        Prompt {
            model: "openai/gpt-5.5".to_string(),
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
    fn codex_adapter_records_previous_response_id_visibility_gap() {
        let prompt = prompt();
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({
            "previous_response_id": "resp_123",
            "input": "continue"
        });
        let ir = CodexResponsesExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::Responses,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::Codex);
        assert_eq!(ir.session.key.as_deref(), Some("resp_123"));
        assert_eq!(ir.session.confidence, SessionConfidence::Medium);
        assert!(
            ir.evidence.iter().any(|e| {
                e.kind == "server_side_context_gap" && e.level == EvidenceLevel::Missing
            })
        );
    }

    #[test]
    fn codex_header_session_wins_over_previous_response_id() {
        let prompt = prompt();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-workflow-session",
            HeaderValue::from_static("bench-session-1"),
        );
        let raw_body = serde_json::json!({
            "previous_response_id": "resp_123",
            "input": "continue"
        });
        let ir = CodexResponsesExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::Responses,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::Codex);
        assert_eq!(ir.session.key.as_deref(), Some("bench-session-1"));
        assert_eq!(ir.session.confidence, SessionConfidence::High);
        assert_eq!(
            ir.session.source.as_deref(),
            Some("header.x-bitrouter-workflow-session")
        );
        assert!(
            ir.evidence.iter().any(|e| {
                e.kind == "server_side_context_gap" && e.level == EvidenceLevel::Missing
            })
        );
    }
}
