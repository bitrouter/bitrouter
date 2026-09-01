use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use bitrouter_sdk::language_model::types::{Content, Message, Role};
use sha2::{Digest, Sha256};

use crate::workflow_state::extractors::{ExtractorInput, adapter_session_hints};
use crate::workflow_state::ir::{
    AgentRole, ContextTransition, Evidence, EvidenceLevel, SessionConfidence, SessionSignal,
    WorkflowIdentity,
};

const WORKFLOW_SESSION_HEADER: &str = "x-bitrouter-workflow-session";
const MAX_TRACKED_PARENT_SESSIONS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct ResolvedSessionSignal {
    pub signal: SessionSignal,
    pub evidence: Vec<Evidence>,
}

/// Per-router context-epoch state. Keys include benchmark run, trial, and
/// parent session so interleaved benchmark trials cannot share epochs.
#[derive(Default)]
pub struct WorkflowIdentityTracker {
    states: Mutex<TrackerStates>,
}

#[derive(Default)]
struct TrackerStates {
    epochs: HashMap<String, EpochState>,
    insertion_order: VecDeque<String>,
}

#[derive(Default)]
struct EpochState {
    epoch: u32,
    compaction_active: bool,
}

/// Resolve structured workflow identity, applying explicit identity headers
/// before Terminus 2 prompt inference and stateful epoch tracking.
pub fn resolve_workflow_identity(
    input: &ExtractorInput<'_>,
    tracker: &WorkflowIdentityTracker,
) -> WorkflowIdentity {
    let session = resolve_session_signal(input).signal;
    let adapter_hints = adapter_session_hints(input);
    let benchmark_run_id = header_value(input, "x-bitrouter-benchmark-run-id");
    let trial_id = header_value(input, "x-bitrouter-trial-id");
    let explicit_parent = header_value(input, "x-bitrouter-parent-session-id");
    let explicit_agent = header_value(input, "x-bitrouter-agent-session-id");
    let explicit_role =
        header_value(input, "x-bitrouter-agent-role").map(|value| parse_role(&value));
    let explicit_epoch = header_value(input, "x-bitrouter-context-epoch")
        .and_then(|value| value.parse::<u32>().ok());
    let explicit_transition =
        header_value(input, "x-bitrouter-context-transition").map(|value| parse_transition(&value));
    let explicit_fingerprint = header_value(input, "x-bitrouter-session-fingerprint");
    let role = explicit_role
        .or(adapter_hints.inferred_role)
        .unwrap_or(AgentRole::Unknown);
    let parent_session_id = explicit_parent
        .or(adapter_hints.parent_session_id)
        .or_else(|| session.key.clone());
    let state_key = parent_session_id.as_ref().map(|parent| {
        format!(
            "{}|{}|{parent}",
            benchmark_run_id.as_deref().unwrap_or("-"),
            trial_id.as_deref().unwrap_or("-")
        )
    });

    let (context_epoch, transition) = match (explicit_epoch, explicit_transition, state_key) {
        (Some(epoch), transition, Some(key)) => {
            update_explicit_epoch(tracker, key, epoch, role);
            (epoch, transition.unwrap_or(ContextTransition::None))
        }
        (Some(epoch), transition, None) => (epoch, transition.unwrap_or(ContextTransition::None)),
        (None, transition, Some(key)) if adapter_hints.tracks_context_epoch => {
            let (epoch, inferred) =
                advance_epoch(tracker, key, role, adapter_hints.observed_context_epoch);
            (epoch, transition.unwrap_or(inferred))
        }
        (None, transition, None) | (None, transition, Some(_)) => {
            (0, transition.unwrap_or(ContextTransition::None))
        }
    };
    let fingerprint = explicit_fingerprint.unwrap_or_else(|| {
        identity_fingerprint(
            benchmark_run_id.as_deref(),
            trial_id.as_deref(),
            parent_session_id.as_deref(),
            context_epoch,
        )
    });
    let agent_session_id = explicit_agent.or_else(|| session.key.clone()).or_else(|| {
        parent_session_id
            .as_ref()
            .map(|parent| format!("{parent}:{}:{context_epoch}", role.as_str()))
    });
    let explicit = input.headers.contains_key("x-bitrouter-parent-session-id")
        || input.headers.contains_key("x-bitrouter-agent-session-id")
        || input.headers.contains_key("x-bitrouter-agent-role")
        || input.headers.contains_key("x-bitrouter-context-epoch")
        || input
            .headers
            .contains_key("x-bitrouter-session-fingerprint");

    WorkflowIdentity {
        benchmark_run_id,
        trial_id,
        agent_session_id,
        parent_session_id,
        role,
        context_epoch,
        transition,
        fingerprint,
        source: if explicit {
            "explicit_headers".to_string()
        } else if adapter_hints.tracks_context_epoch {
            "adapter_session".to_string()
        } else {
            "inferred".to_string()
        },
        confidence: session.confidence,
    }
}

fn update_explicit_epoch(
    tracker: &WorkflowIdentityTracker,
    key: String,
    epoch: u32,
    role: AgentRole,
) {
    let mut states = match tracker.states.lock() {
        Ok(states) => states,
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = tracked_epoch_state(&mut states, key);
    *state = EpochState {
        epoch,
        compaction_active: matches!(
            role,
            AgentRole::Summary | AgentRole::Questions | AgentRole::Answers
        ),
    };
}

fn advance_epoch(
    tracker: &WorkflowIdentityTracker,
    key: String,
    role: AgentRole,
    observed_epoch: Option<u32>,
) -> (u32, ContextTransition) {
    let mut states = match tracker.states.lock() {
        Ok(states) => states,
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = tracked_epoch_state(&mut states, key);
    let transition = match role {
        AgentRole::Summary => {
            let epoch = observed_epoch.unwrap_or_else(|| state.epoch.saturating_add(1));
            let starts_compaction = !state.compaction_active || epoch > state.epoch;
            state.epoch = state.epoch.max(epoch);
            if starts_compaction {
                state.compaction_active = true;
                ContextTransition::CompactionStart
            } else {
                ContextTransition::CompactionContinuation
            }
        }
        AgentRole::Questions | AgentRole::Answers => {
            if let Some(epoch) = observed_epoch {
                state.epoch = state.epoch.max(epoch);
            }
            state.compaction_active = true;
            ContextTransition::CompactionContinuation
        }
        AgentRole::Main => {
            let observed_advance = observed_epoch.is_some_and(|epoch| epoch > state.epoch);
            if let Some(epoch) = observed_epoch {
                state.epoch = state.epoch.max(epoch);
            }
            if state.compaction_active || observed_advance {
                state.compaction_active = false;
                ContextTransition::MainResume
            } else {
                ContextTransition::None
            }
        }
        AgentRole::Unknown => ContextTransition::None,
    };
    (observed_epoch.unwrap_or(state.epoch), transition)
}

fn tracked_epoch_state(states: &mut TrackerStates, key: String) -> &mut EpochState {
    if !states.epochs.contains_key(&key) {
        while states.epochs.len() >= MAX_TRACKED_PARENT_SESSIONS {
            let Some(oldest) = states.insertion_order.pop_front() else {
                break;
            };
            states.epochs.remove(&oldest);
        }
        states.insertion_order.push_back(key.clone());
    }
    states.epochs.entry(key).or_default()
}

fn parse_role(value: &str) -> AgentRole {
    match value.trim().to_ascii_lowercase().as_str() {
        "main" => AgentRole::Main,
        "summary" => AgentRole::Summary,
        "questions" => AgentRole::Questions,
        "answers" => AgentRole::Answers,
        _ => AgentRole::Unknown,
    }
}

fn parse_transition(value: &str) -> ContextTransition {
    match value.trim().to_ascii_lowercase().as_str() {
        "compaction_start" | "compaction-start" => ContextTransition::CompactionStart,
        "compaction_continuation" | "compaction-continuation" => {
            ContextTransition::CompactionContinuation
        }
        "main_resume" | "main-resume" => ContextTransition::MainResume,
        _ => ContextTransition::None,
    }
}

pub(crate) fn identity_fingerprint(
    benchmark_run_id: Option<&str>,
    trial_id: Option<&str>,
    parent_session_id: Option<&str>,
    context_epoch: u32,
) -> String {
    let material = format!(
        "terminus_2|{}|{}|{}|{context_epoch}",
        benchmark_run_id.unwrap_or("-"),
        trial_id.unwrap_or("-"),
        parent_session_id.unwrap_or("-")
    );
    format!(
        "sha256:{}",
        hex::encode(Sha256::digest(material.as_bytes()))
    )
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

    let adapter_hints = adapter_session_hints(input);
    if let (Some(session_key), Some(session_source)) =
        (adapter_hints.session_key, adapter_hints.session_source)
    {
        return resolved(
            session_key,
            adapter_hints.session_confidence,
            session_source,
            EvidenceLevel::Observed,
            0.8,
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
    use crate::workflow_state::ir::{
        AgentRole, ContextTransition, HarnessId, ProtocolKind, SessionConfidence,
    };
    use crate::workflow_state::session::{
        MAX_TRACKED_PARENT_SESSIONS, WorkflowIdentityTracker, advance_epoch,
        resolve_session_signal, resolve_workflow_identity,
    };

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PureApiCase {
        name: String,
        protocol: String,
        harness_hint: String,
        headers: std::collections::BTreeMap<String, String>,
        raw_body: serde_json::Value,
        first_user_text: String,
        expected: PureApiExpected,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PureApiExpected {
        key: String,
        confidence: String,
        source: String,
        agent_session_id: String,
        parent_session_id: String,
        identity_source: String,
        fingerprint: String,
    }

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
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219"),
        );
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

    #[test]
    fn pure_model_api_session_compatibility_matrix() -> anyhow::Result<()> {
        let cases: Vec<PureApiCase> = serde_json::from_str(include_str!(
            "../../tests/fixtures/workflow_state/pure_model_api_sessions.json"
        ))?;

        for case in cases {
            let protocol = match case.protocol.as_str() {
                "chat_completions" => ProtocolKind::ChatCompletions,
                "messages" => ProtocolKind::Messages,
                "responses" => ProtocolKind::Responses,
                other => anyhow::bail!("{}: unknown protocol {other}", case.name),
            };
            let harness_hint = match case.harness_hint.as_str() {
                "generic" => HarnessId::Generic,
                "hermes" => HarnessId::Hermes,
                other => anyhow::bail!("{}: unknown harness {other}", case.name),
            };
            let expected_confidence = match case.expected.confidence.as_str() {
                "low" => SessionConfidence::Low,
                "medium" => SessionConfidence::Medium,
                "high" => SessionConfidence::High,
                other => anyhow::bail!("{}: unknown confidence {other}", case.name),
            };
            let mut headers = HeaderMap::new();
            for (name, value) in &case.headers {
                headers.insert(
                    http::header::HeaderName::from_bytes(name.as_bytes())?,
                    HeaderValue::from_str(value)?,
                );
            }
            let prompt = prompt(&case.first_user_text);
            let input = ExtractorInput {
                harness_hint: Some(harness_hint),
                protocol_hint: protocol,
                headers: &headers,
                raw_body: &case.raw_body,
                prompt: &prompt,
            };

            let signal = resolve_session_signal(&input).signal;
            assert_eq!(
                signal.key.as_deref(),
                Some(case.expected.key.as_str()),
                "{}: session key",
                case.name
            );
            assert_eq!(
                signal.confidence, expected_confidence,
                "{}: session confidence",
                case.name
            );
            assert_eq!(
                signal.source.as_deref(),
                Some(case.expected.source.as_str()),
                "{}: session source",
                case.name
            );

            let identity = resolve_workflow_identity(&input, &WorkflowIdentityTracker::default());
            assert_eq!(
                identity.agent_session_id.as_deref(),
                Some(case.expected.agent_session_id.as_str()),
                "{}: agent identity",
                case.name
            );
            assert_eq!(
                identity.parent_session_id.as_deref(),
                Some(case.expected.parent_session_id.as_str()),
                "{}: parent identity",
                case.name
            );
            assert_eq!(identity.role, AgentRole::Unknown, "{}: role", case.name);
            assert_eq!(identity.context_epoch, 0, "{}: epoch", case.name);
            assert_eq!(
                identity.transition,
                ContextTransition::None,
                "{}: transition",
                case.name
            );
            assert_eq!(
                identity.source, case.expected.identity_source,
                "{}: identity source",
                case.name
            );
            assert_eq!(
                identity.confidence, expected_confidence,
                "{}: identity confidence",
                case.name
            );
            assert_eq!(
                identity.fingerprint, case.expected.fingerprint,
                "{}: identity fingerprint",
                case.name
            );
        }
        Ok(())
    }

    #[test]
    fn workflow_identity_tracker_evicts_old_parent_sessions() {
        let tracker = WorkflowIdentityTracker::default();
        for index in 0..=MAX_TRACKED_PARENT_SESSIONS {
            advance_epoch(
                &tracker,
                format!("run|trial|parent-{index}"),
                crate::workflow_state::ir::AgentRole::Main,
                None,
            );
        }

        let states = tracker
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(states.epochs.len(), MAX_TRACKED_PARENT_SESSIONS);
        assert!(!states.epochs.contains_key("run|trial|parent-0"));
        assert!(
            states
                .epochs
                .contains_key(&format!("run|trial|parent-{MAX_TRACKED_PARENT_SESSIONS}"))
        );
    }
}
