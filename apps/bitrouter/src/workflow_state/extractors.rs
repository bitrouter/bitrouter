use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::Prompt;

use crate::workflow_state::ir::{HarnessId, ProtocolKind, WorkflowStateIR};

pub mod claude_code;
pub mod codex;
pub mod generic;
pub mod hermes;
pub mod openclaw;
pub mod terminus_2;

pub trait WorkflowStateExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR;
}

pub struct ExtractorInput<'a> {
    pub harness_hint: Option<HarnessId>,
    pub protocol_hint: ProtocolKind,
    pub headers: &'a HeaderMap,
    pub raw_body: &'a serde_json::Value,
    pub prompt: &'a Prompt,
}

pub fn extract_workflow_state(input: &ExtractorInput<'_>) -> WorkflowStateIR {
    use claude_code::ClaudeCodeExtractor;
    use codex::CodexResponsesExtractor;
    use generic::GenericPromptExtractor;
    use hermes::HermesExtractor;
    use openclaw::OpenClawExtractor;
    use terminus_2::Terminus2Extractor;

    match input.harness_hint.as_ref().unwrap_or(&HarnessId::Generic) {
        HarnessId::Hermes => HermesExtractor.extract(input),
        HarnessId::ClaudeCode => ClaudeCodeExtractor.extract(input),
        HarnessId::Codex => CodexResponsesExtractor.extract(input),
        HarnessId::Terminus2 => Terminus2Extractor.extract(input),
        HarnessId::OpenClaw => OpenClawExtractor.extract(input),
        HarnessId::Generic | HarnessId::Unknown => GenericPromptExtractor.extract(input),
    }
}
