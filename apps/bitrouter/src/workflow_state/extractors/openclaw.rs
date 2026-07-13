use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, ProtocolKind, WorkflowStateIR,
};

pub struct OpenClawExtractor;

impl WorkflowStateExtractor for OpenClawExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(&ExtractorInput {
            harness_hint: Some(HarnessId::OpenClaw),
            protocol_hint: ProtocolKind::OpenClawRuntime,
            headers: input.headers,
            raw_body: input.raw_body,
            prompt: input.prompt,
        });
        if let Some(runtime_id) = input
            .raw_body
            .get("agentRuntime")
            .and_then(|r| r.get("id"))
            .and_then(|v| v.as_str())
        {
            ir.active_workflow = Some(runtime_id.to_string());
            ir.evidence.push(Evidence {
                kind: "openclaw_runtime_metadata".to_string(),
                value: runtime_id.to_string(),
                confidence: 0.55,
                level: EvidenceLevel::DocumentedStub,
            });
        } else {
            ir.evidence.push(Evidence {
                kind: "openclaw_runtime_metadata".to_string(),
                value: "missing real trace metadata".to_string(),
                confidence: 0.2,
                level: EvidenceLevel::DocumentedStub,
            });
        }
        ir.confidence = ir.confidence.min(0.55);
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

    use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
    use crate::workflow_state::ir::{EvidenceLevel, HarnessId, ProtocolKind};

    fn prompt() -> Prompt {
        Prompt {
            model: "openclaw-runtime-model".to_string(),
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
    fn openclaw_adapter_marks_documented_stub_until_real_trace() {
        let prompt = prompt();
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({
            "agentRuntime": {"id": "openclaw.default"},
            "runtimePlan": {"tools": ["shell", "edit"]}
        });
        let ir = OpenClawExtractor.extract(&ExtractorInput {
            harness_hint: None,
            protocol_hint: ProtocolKind::OpenClawRuntime,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });
        assert_eq!(ir.harness_id, HarnessId::OpenClaw);
        assert!(ir.evidence.iter().any(|e| {
            e.kind == "openclaw_runtime_metadata" && e.level == EvidenceLevel::DocumentedStub
        }));
    }
}
