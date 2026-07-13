use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, ProtocolKind, SessionConfidence, WorkflowStateIR,
};

pub struct HermesExtractor;

impl WorkflowStateExtractor for HermesExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: Some(HarnessId::Hermes),
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: input.headers,
            raw_body: input.raw_body,
            prompt: input.prompt,
        });
        if input.raw_body.get("messages").is_some() {
            ir.evidence.push(Evidence {
                kind: "prompt_visibility".to_string(),
                value: "full_messages_present".to_string(),
                confidence: 0.9,
                level: EvidenceLevel::Observed,
            });
        }
        if ir.session.key.is_none()
            && let Some(job_id) = input
                .raw_body
                .get("metadata")
                .and_then(|m| m.get("job_id"))
                .and_then(|v| v.as_str())
        {
            ir.session.key = Some(job_id.to_string());
            ir.session.confidence = SessionConfidence::Medium;
            ir.session.source = Some("raw_body.metadata.job_id".to_string());
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
    use crate::workflow_state::ir::{HarnessId, ProtocolKind, SessionConfidence};

    fn prompt() -> Prompt {
        Prompt {
            model: "bitrouter-mvp-alias".to_string(),
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
    fn hermes_adapter_preserves_full_prompt_visibility() {
        let prompt = prompt();
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({
            "messages": [{"role": "user", "content": "inspect"}]
        });
        let ir = HermesExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::Hermes);
        assert!(
            ir.evidence
                .iter()
                .any(|e| { e.kind == "prompt_visibility" && e.value == "full_messages_present" })
        );
    }

    #[test]
    fn hermes_header_session_wins_over_metadata_job_id() {
        let prompt = prompt();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-workflow-session",
            HeaderValue::from_static("bench-session-1"),
        );
        let raw_body = serde_json::json!({
            "messages": [{"role": "user", "content": "inspect"}],
            "metadata": { "job_id": "hermes-job-1" }
        });
        let ir = HermesExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::Hermes);
        assert_eq!(ir.session.key.as_deref(), Some("bench-session-1"));
        assert_eq!(ir.session.confidence, SessionConfidence::High);
        assert_eq!(
            ir.session.source.as_deref(),
            Some("header.x-bitrouter-workflow-session")
        );
    }
}
