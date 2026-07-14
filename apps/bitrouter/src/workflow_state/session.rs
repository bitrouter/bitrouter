use bitrouter_sdk::language_model::types::{Content, Message, Role};
use sha2::{Digest, Sha256};

use crate::workflow_state::extractors::ExtractorInput;
use crate::workflow_state::ir::{
    Evidence, EvidenceLevel, HarnessId, SessionConfidence, SessionSignal,
};

const WORKFLOW_SESSION_HEADER: &str = "x-bitrouter-workflow-session";

#[derive(Debug, Clone)]
pub struct ResolvedSessionSignal {
    pub signal: SessionSignal,
    pub evidence: Vec<Evidence>,
}

pub fn resolve_session_signal(input: &ExtractorInput<'_>) -> ResolvedSessionSignal {
    if let Some(value) = header_value(input, WORKFLOW_SESSION_HEADER) {
        return resolved(
            value,
            SessionConfidence::High,
            "header.x-bitrouter-workflow-session",
            EvidenceLevel::Observed,
            0.99,
        );
    }

    if matches!(input.harness_hint, Some(HarnessId::ClaudeCode)) {
        if let Some(session_id) = claude_metadata_session_id(input.raw_body) {
            return resolved(
                session_id,
                SessionConfidence::High,
                "raw_body.metadata.user_id.session_id",
                EvidenceLevel::Observed,
                0.98,
            );
        }
        if let Some(user_id) = metadata_str(input.raw_body, "user_id") {
            return resolved(
                user_id,
                SessionConfidence::Low,
                "raw_body.metadata.user_id",
                EvidenceLevel::Observed,
                0.55,
            );
        }
    }

    if matches!(input.harness_hint, Some(HarnessId::Hermes))
        && let Some(job_id) = metadata_str(input.raw_body, "job_id")
    {
        return resolved(
            job_id,
            SessionConfidence::Medium,
            "raw_body.metadata.job_id",
            EvidenceLevel::Observed,
            0.8,
        );
    }

    if matches!(input.harness_hint, Some(HarnessId::Codex))
        && let Some(previous_response_id) = input
            .raw_body
            .get("previous_response_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.trim().is_empty())
    {
        return resolved(
            previous_response_id.to_string(),
            SessionConfidence::Medium,
            "raw_body.previous_response_id",
            EvidenceLevel::Observed,
            0.75,
        );
    }

    if let Some(hash) = first_user_message_hash(input.prompt.messages.as_slice()) {
        return resolved(
            format!("prompt:{hash}"),
            SessionConfidence::Low,
            "prompt.first_user_message_sha256",
            EvidenceLevel::Inferred,
            0.45,
        );
    }

    ResolvedSessionSignal {
        signal: SessionSignal::default(),
        evidence: Vec::new(),
    }
}

fn header_value(input: &ExtractorInput<'_>, name: &str) -> Option<String> {
    input
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn claude_metadata_session_id(raw_body: &serde_json::Value) -> Option<String> {
    let user_id = metadata_str(raw_body, "user_id")?;
    serde_json::from_str::<serde_json::Value>(&user_id)
        .ok()
        .and_then(|value| {
            value
                .get("session_id")
                .and_then(|session_id| session_id.as_str())
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
                .map(ToString::to_string)
        })
}

fn metadata_str(raw_body: &serde_json::Value, key: &str) -> Option<String> {
    raw_body
        .get("metadata")
        .and_then(|metadata| metadata.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn first_user_message_hash(messages: &[Message]) -> Option<String> {
    let text = messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(message_text)?
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let digest = Sha256::digest(text.as_bytes());
    Some(hex::encode(&digest[..12]))
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn resolved(
    key: String,
    confidence: SessionConfidence,
    source: &str,
    level: EvidenceLevel,
    evidence_confidence: f32,
) -> ResolvedSessionSignal {
    ResolvedSessionSignal {
        signal: SessionSignal {
            key: Some(key),
            confidence,
            source: Some(source.to_string()),
        },
        evidence: vec![Evidence {
            kind: "session_signal".to_string(),
            value: source.to_string(),
            confidence: evidence_confidence,
            level,
        }],
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::language_model::types::{
        GenerationParams, Message, Prompt, ProviderMetadata, Role,
    };
    use http::HeaderValue;

    use crate::workflow_state::extractors::ExtractorInput;
    use crate::workflow_state::ir::{HarnessId, ProtocolKind, SessionConfidence};
    use crate::workflow_state::session::resolve_session_signal;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            model: "test-model".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message::text(Role::User, text)],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn workflow_session_header_is_high_confidence_and_wins() {
        let prompt = prompt("inspect");
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-workflow-session",
            HeaderValue::from_static("bench-run-1"),
        );
        let raw_body = serde_json::json!({
            "metadata": {
                "job_id": "hermes-job-1",
                "user_id": "{\"session_id\":\"claude-session-1\"}"
            },
            "previous_response_id": "resp_123"
        });

        let resolved = resolve_session_signal(&ExtractorInput {
            harness_hint: Some(HarnessId::Hermes),
            protocol_hint: ProtocolKind::ChatCompletions,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });

        assert_eq!(resolved.signal.key.as_deref(), Some("bench-run-1"));
        assert_eq!(resolved.signal.confidence, SessionConfidence::High);
        assert_eq!(
            resolved.signal.source.as_deref(),
            Some("header.x-bitrouter-workflow-session")
        );
        assert!(resolved.evidence.iter().any(|e| {
            e.kind == "session_signal" && e.value == "header.x-bitrouter-workflow-session"
        }));
    }

    #[test]
    fn claude_metadata_user_id_json_session_id_is_high_confidence() {
        let prompt = prompt("inspect");
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({
            "metadata": {
                "user_id": "{\"device_id\":\"device-1\",\"account_uuid\":\"acct-1\",\"session_id\":\"00000000-0000-4000-8000-000000000001\"}"
            }
        });

        let resolved = resolve_session_signal(&ExtractorInput {
            harness_hint: Some(HarnessId::ClaudeCode),
            protocol_hint: ProtocolKind::Messages,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });

        assert_eq!(
            resolved.signal.key.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(resolved.signal.confidence, SessionConfidence::High);
        assert_eq!(
            resolved.signal.source.as_deref(),
            Some("raw_body.metadata.user_id.session_id")
        );
    }

    #[test]
    fn first_user_message_hash_is_low_confidence_fallback() {
        let prompt = prompt("same benchmark task");
        let headers = HeaderMap::new();
        let raw_body = serde_json::json!({});

        let resolved = resolve_session_signal(&ExtractorInput {
            harness_hint: Some(HarnessId::Codex),
            protocol_hint: ProtocolKind::Responses,
            headers: &headers,
            raw_body: &raw_body,
            prompt: &prompt,
        });

        assert!(
            resolved
                .signal
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with("prompt:"))
        );
        assert_eq!(resolved.signal.confidence, SessionConfidence::Low);
        assert_eq!(
            resolved.signal.source.as_deref(),
            Some("prompt.first_user_message_sha256")
        );
    }
}
