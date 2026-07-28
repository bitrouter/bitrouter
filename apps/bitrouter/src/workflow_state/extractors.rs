use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::language_model::types::Prompt;

use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, ProtocolKind, WorkflowStateIR,
};

pub mod claude_code;
pub mod codex;
pub mod generic;
pub mod hermes;
pub mod openclaw;
pub mod smithers;
pub mod terminus_2;

pub trait WorkflowStateExtractor {
    fn detect(&self, _input: &ExtractorInput<'_>) -> Option<TraceAdapterMatch> {
        None
    }

    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR;
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceAdapterMatch {
    pub source: HarnessId,
    pub confidence: f32,
    pub evidence_kind: &'static str,
}

pub(crate) struct AdapterSessionHints {
    pub session_key: Option<String>,
    pub session_confidence: crate::workflow_state::ir::SessionConfidence,
    pub session_source: Option<&'static str>,
    pub terminus_identity: Option<terminus_2::TerminusSessionIdentity>,
    pub tracks_context_epoch: bool,
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
    use smithers::SmithersExtractor;
    use terminus_2::Terminus2Extractor;

    let adapter = detect_trace_adapter(input);
    let selected_input = ExtractorInput {
        harness_hint: Some(adapter.source.clone()),
        protocol_hint: adapter_protocol_hint(input, &adapter),
        headers: input.headers,
        raw_body: input.raw_body,
        prompt: input.prompt,
    };
    let mut ir = match adapter.source {
        HarnessId::Hermes => HermesExtractor.extract(&selected_input),
        HarnessId::ClaudeCode => ClaudeCodeExtractor.extract(&selected_input),
        HarnessId::Codex => CodexResponsesExtractor.extract(&selected_input),
        HarnessId::Smithers => SmithersExtractor.extract(&selected_input),
        HarnessId::Terminus2 => Terminus2Extractor.extract(&selected_input),
        HarnessId::OpenClaw => OpenClawExtractor.extract(&selected_input),
        HarnessId::Generic | HarnessId::Unknown => GenericPromptExtractor.extract(&selected_input),
    };
    ir.evidence.push(Evidence {
        kind: "trace_adapter".to_string(),
        value: adapter.evidence_kind.to_string(),
        confidence: adapter.confidence,
        level: if adapter.confidence >= 0.9 {
            EvidenceLevel::Observed
        } else {
            EvidenceLevel::Inferred
        },
    });
    ir
}

pub fn detect_trace_adapter(input: &ExtractorInput<'_>) -> TraceAdapterMatch {
    use claude_code::ClaudeCodeExtractor;
    use codex::CodexResponsesExtractor;
    use hermes::HermesExtractor;
    use openclaw::OpenClawExtractor;
    use smithers::SmithersExtractor;
    use terminus_2::Terminus2Extractor;

    let native_matches = [
        ClaudeCodeExtractor.detect(input),
        CodexResponsesExtractor.detect(input),
        HermesExtractor.detect(input),
        OpenClawExtractor.detect(input),
        Terminus2Extractor.detect(input),
        SmithersExtractor.detect(input),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let first_source = native_matches
        .first()
        .map(|candidate| candidate.source.clone());
    if let Some(source) = first_source {
        if native_matches
            .iter()
            .all(|candidate| candidate.source == source)
            && let Some(best_match) = native_matches
                .into_iter()
                .max_by(|left, right| left.confidence.total_cmp(&right.confidence))
        {
            return best_match;
        }
        return TraceAdapterMatch {
            source: HarnessId::Generic,
            confidence: 0.0,
            evidence_kind: "conflicting_native_evidence",
        };
    }

    if let Some(source) = input.harness_hint.clone()
        && !matches!(source, HarnessId::Generic | HarnessId::Unknown)
    {
        return TraceAdapterMatch {
            source,
            confidence: 0.1,
            evidence_kind: "compatibility_harness_hint",
        };
    }

    TraceAdapterMatch {
        source: HarnessId::Generic,
        confidence: 0.0,
        evidence_kind: "generic_fallback",
    }
}

pub fn adapter_protocol_hint(
    input: &ExtractorInput<'_>,
    adapter: &TraceAdapterMatch,
) -> ProtocolKind {
    if input.protocol_hint != ProtocolKind::Unknown {
        return input.protocol_hint.clone();
    }
    match adapter.source {
        HarnessId::ClaudeCode => ProtocolKind::Messages,
        HarnessId::Codex => ProtocolKind::Responses,
        HarnessId::Hermes | HarnessId::Terminus2 => ProtocolKind::ChatCompletions,
        HarnessId::OpenClaw => ProtocolKind::OpenClawRuntime,
        HarnessId::Generic | HarnessId::Smithers | HarnessId::Unknown => ProtocolKind::Unknown,
    }
}

pub fn parse_compatibility_harness(value: &str) -> Option<HarnessId> {
    match value.trim().to_ascii_lowercase().as_str() {
        "generic" => Some(HarnessId::Generic),
        "hermes" => Some(HarnessId::Hermes),
        "claude" | "claude_code" | "claude-code" => Some(HarnessId::ClaudeCode),
        "codex" => Some(HarnessId::Codex),
        "smithers" => Some(HarnessId::Smithers),
        "terminus_2" | "terminus-2" | "terminus2" => Some(HarnessId::Terminus2),
        "openclaw" | "open_claw" | "open-claw" => Some(HarnessId::OpenClaw),
        "unknown" => Some(HarnessId::Unknown),
        _ => None,
    }
}

pub(crate) fn adapter_session_hints(input: &ExtractorInput<'_>) -> AdapterSessionHints {
    use crate::workflow_state::ir::SessionConfidence;

    let adapter = detect_trace_adapter(input);
    match adapter.source {
        HarnessId::Codex => AdapterSessionHints {
            session_key: codex::previous_response_id(input.raw_body),
            session_confidence: SessionConfidence::Medium,
            session_source: Some("raw_body.previous_response_id"),
            terminus_identity: None,
            tracks_context_epoch: false,
        },
        HarnessId::ClaudeCode => {
            let session_id = claude_code::metadata_session_id(input.raw_body);
            let user_id = claude_code::metadata_user_id(input.raw_body);
            AdapterSessionHints {
                session_key: session_id.or(user_id),
                session_confidence: if claude_code::metadata_session_id(input.raw_body).is_some() {
                    SessionConfidence::High
                } else {
                    SessionConfidence::Low
                },
                session_source: if claude_code::metadata_session_id(input.raw_body).is_some() {
                    Some("raw_body.metadata.user_id.session_id")
                } else {
                    Some("raw_body.metadata.user_id")
                },
                terminus_identity: None,
                tracks_context_epoch: false,
            }
        }
        HarnessId::Hermes => AdapterSessionHints {
            session_key: hermes::metadata_job_id(input.raw_body),
            session_confidence: SessionConfidence::Medium,
            session_source: Some("raw_body.metadata.job_id"),
            terminus_identity: None,
            tracks_context_epoch: false,
        },
        HarnessId::Terminus2 => {
            let session_key = terminus_2::session_id(input);
            let terminus_identity = session_key
                .as_deref()
                .map(terminus_2::parse_session_identity);
            AdapterSessionHints {
                session_key,
                session_confidence: SessionConfidence::High,
                session_source: Some("adapter.terminus_2.session_id"),
                terminus_identity,
                tracks_context_epoch: true,
            }
        }
        HarnessId::Generic | HarnessId::Smithers | HarnessId::OpenClaw | HarnessId::Unknown => {
            AdapterSessionHints {
                session_key: None,
                session_confidence: SessionConfidence::None,
                session_source: None,
                terminus_identity: None,
                tracks_context_epoch: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        GenerationParams, Message, Prompt, ProviderMetadata, Role,
    };
    use http::HeaderValue;

    use super::{ExtractorInput, detect_trace_adapter};
    use crate::workflow_state::ir::{HarnessId, ProtocolKind};

    fn prompt() -> Prompt {
        Prompt {
            model: "inbound".to_string(),
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
    fn detects_native_agent_traffic_before_compatibility_hints() {
        struct Case {
            name: &'static str,
            protocol: ProtocolKind,
            headers: Vec<(&'static str, &'static str)>,
            raw_body: serde_json::Value,
            system: Option<&'static str>,
            expected: HarnessId,
            evidence_kind: &'static str,
        }

        let terminus_contract = "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON commands with task_complete.";
        let cases = vec![
            Case {
                name: "claude code beta",
                protocol: ProtocolKind::Messages,
                headers: vec![("anthropic-beta", "claude-code-20250219")],
                raw_body: serde_json::json!({}),
                system: None,
                expected: HarnessId::ClaudeCode,
                evidence_kind: "anthropic_beta",
            },
            Case {
                name: "codex responses user agent",
                protocol: ProtocolKind::Responses,
                headers: vec![("user-agent", "codex-cli/0.48")],
                raw_body: serde_json::json!({}),
                system: None,
                expected: HarnessId::Codex,
                evidence_kind: "responses_codex_user_agent",
            },
            Case {
                name: "codex responses continuation",
                protocol: ProtocolKind::Responses,
                headers: vec![],
                raw_body: serde_json::json!({"previous_response_id": "resp_123"}),
                system: None,
                expected: HarnessId::Codex,
                evidence_kind: "responses_previous_response_id",
            },
            Case {
                name: "hermes metadata",
                protocol: ProtocolKind::ChatCompletions,
                headers: vec![],
                raw_body: serde_json::json!({"metadata": {"job_id": "job-123"}}),
                system: None,
                expected: HarnessId::Hermes,
                evidence_kind: "hermes_metadata",
            },
            Case {
                name: "openclaw runtime",
                protocol: ProtocolKind::OpenClawRuntime,
                headers: vec![],
                raw_body: serde_json::json!({"agentRuntime": {"id": "openclaw.default"}}),
                system: None,
                expected: HarnessId::OpenClaw,
                evidence_kind: "agent_runtime",
            },
            Case {
                name: "terminus prompt contract",
                protocol: ProtocolKind::ChatCompletions,
                headers: vec![],
                raw_body: serde_json::json!({}),
                system: Some(terminus_contract),
                expected: HarnessId::Terminus2,
                evidence_kind: "terminus_2_prompt_contract",
            },
            Case {
                name: "smithers metadata",
                protocol: ProtocolKind::ChatCompletions,
                headers: vec![("x-smithers-workflow-id", "repair")],
                raw_body: serde_json::json!({}),
                system: None,
                expected: HarnessId::Smithers,
                evidence_kind: "smithers_metadata",
            },
            Case {
                name: "generic fallback",
                protocol: ProtocolKind::ChatCompletions,
                headers: vec![],
                raw_body: serde_json::json!({}),
                system: None,
                expected: HarnessId::Generic,
                evidence_kind: "generic_fallback",
            },
        ];

        for case in cases {
            let mut headers = HeaderMap::new();
            for (name, value) in case.headers {
                headers.insert(name, HeaderValue::from_static(value));
            }
            let mut prompt = prompt();
            prompt.system = case.system.map(str::to_string);
            let input = ExtractorInput {
                harness_hint: Some(HarnessId::Generic),
                protocol_hint: case.protocol,
                headers: &headers,
                raw_body: &case.raw_body,
                prompt: &prompt,
            };
            let detected = detect_trace_adapter(&input);
            assert_eq!(detected.source, case.expected, "{}", case.name);
            assert_eq!(detected.evidence_kind, case.evidence_kind, "{}", case.name);
        }
    }

    #[test]
    fn conflicting_native_evidence_falls_back_to_generic() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219"),
        );
        headers.insert("user-agent", HeaderValue::from_static("codex-cli/0.48"));
        let prompt = prompt();
        let raw_body = serde_json::json!({});
        let input = ExtractorInput {
            harness_hint: Some(HarnessId::Smithers),
            protocol_hint: ProtocolKind::Responses,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        };

        let detected = detect_trace_adapter(&input);
        assert_eq!(detected.source, HarnessId::Generic);
        assert_eq!(detected.evidence_kind, "conflicting_native_evidence");
    }

    #[test]
    fn native_evidence_outranks_compatibility_harness_hint() {
        let headers = HeaderMap::new();
        let prompt = prompt();
        let raw_body = serde_json::json!({"previous_response_id": "resp_123"});
        let input = ExtractorInput {
            harness_hint: Some(HarnessId::Smithers),
            protocol_hint: ProtocolKind::Responses,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        };

        let detected = detect_trace_adapter(&input);
        assert_eq!(detected.source, HarnessId::Codex);
        assert_eq!(detected.evidence_kind, "responses_previous_response_id");
    }
}
