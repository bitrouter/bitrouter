use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use bitrouter_sdk::language_model::types::{Content, Message, Role};
use sha2::{Digest, Sha256};

use crate::workflow_state::extractors::ExtractorInput;
use crate::workflow_state::ir::{
    AgentRole, ContextTransition, Evidence, EvidenceLevel, HarnessId, SessionConfidence,
    SessionSignal, WorkflowIdentity,
};

const WORKFLOW_SESSION_HEADER: &str = "x-bitrouter-workflow-session";
const MAX_TRACKED_PARENT_SESSIONS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct ResolvedSessionSignal {
    pub signal: SessionSignal,
    pub evidence: Vec<Evidence>,
}

/// Per-router context-epoch state. Keys include benchmark run, trial, and
/// parent session so interleaved Terminal-Bench trials cannot share epochs.
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

struct TerminusSessionIdentity {
    parent_session_id: String,
    role: Option<AgentRole>,
    context_epoch: Option<u32>,
}

/// Resolve structured workflow identity, applying explicit identity headers
/// before Terminus 2 prompt inference and stateful epoch tracking.
pub fn resolve_workflow_identity(
    input: &ExtractorInput<'_>,
    tracker: &WorkflowIdentityTracker,
) -> WorkflowIdentity {
    let session = resolve_session_signal(input).signal;
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
    let terminus_session = if matches!(input.harness_hint, Some(HarnessId::Terminus2)) {
        session.key.as_deref().map(parse_terminus_session_id)
    } else {
        None
    };

    let role = explicit_role
        .or_else(|| terminus_session.as_ref().and_then(|identity| identity.role))
        .unwrap_or_else(|| {
            if matches!(input.harness_hint, Some(HarnessId::Terminus2)) {
                infer_terminus_role(input.prompt)
            } else {
                AgentRole::Unknown
            }
        });
    let parent_session_id = explicit_parent
        .or_else(|| {
            terminus_session
                .as_ref()
                .map(|identity| identity.parent_session_id.clone())
        })
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
        (None, transition, Some(key))
            if matches!(input.harness_hint, Some(HarnessId::Terminus2)) =>
        {
            let observed_epoch = terminus_session
                .as_ref()
                .and_then(|identity| identity.context_epoch);
            let (epoch, inferred) = advance_epoch(tracker, key, role, observed_epoch);
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
        } else if terminus_session.is_some() {
            "terminus_session_id".to_string()
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

fn parse_terminus_session_id(session_id: &str) -> TerminusSessionIdentity {
    if let Some(marker) = session_id.rfind("-summarization-") {
        let parent = &session_id[..marker];
        let suffix = &session_id[marker + "-summarization-".len()..];
        if !parent.is_empty()
            && let Some((epoch, role)) = suffix.split_once('-')
            && let Ok(context_epoch) = epoch.parse::<u32>()
        {
            let role = match role {
                "summary" => Some(AgentRole::Summary),
                "questions" => Some(AgentRole::Questions),
                "answers" => Some(AgentRole::Answers),
                _ => None,
            };
            if role.is_some() {
                return TerminusSessionIdentity {
                    parent_session_id: parent.to_string(),
                    role,
                    context_epoch: Some(context_epoch),
                };
            }
        }
    }
    if let Some(marker) = session_id.rfind("-cont-") {
        let parent = &session_id[..marker];
        let epoch = &session_id[marker + "-cont-".len()..];
        if !parent.is_empty()
            && let Ok(context_epoch) = epoch.parse::<u32>()
        {
            return TerminusSessionIdentity {
                parent_session_id: parent.to_string(),
                role: Some(AgentRole::Main),
                context_epoch: Some(context_epoch),
            };
        }
    }
    TerminusSessionIdentity {
        parent_session_id: session_id.to_string(),
        role: None,
        context_epoch: None,
    }
}

fn infer_terminus_role(prompt: &bitrouter_sdk::language_model::types::Prompt) -> AgentRole {
    let latest_user = prompt
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(message_text)
        .unwrap_or_default();
    let normalized = latest_user.to_ascii_lowercase();
    if normalized.contains("you are about to hand off your work to another ai agent") {
        AgentRole::Summary
    } else if normalized.contains("you are picking up work from a previous ai agent on this task") {
        AgentRole::Questions
    } else if normalized.contains("the next agent has a few questions for you") {
        AgentRole::Answers
    } else if normalized.trim().is_empty() {
        AgentRole::Unknown
    } else {
        AgentRole::Main
    }
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

    // Harbor's Terminus 2 LiteLLM Chat Completions client sends its configured
    // session id both in `X-Session-ID` and as `session_id` in the request body.
    // Header precedence mirrors HTTP middleware behavior; the body remains a
    // fallback for clients that cannot set custom headers.
    // <https://github.com/harbor-framework/harbor/blob/main/src/harbor/llms/lite_llm.py>
    if matches!(input.harness_hint, Some(HarnessId::Terminus2)) {
        if let Some(session_id) = header_value(input, "x-session-id") {
            return resolved(
                session_id,
                SessionConfidence::High,
                "header.x-session-id",
                EvidenceLevel::Observed,
                0.99,
            );
        }
        if let Some(session_id) = input
            .raw_body
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
        {
            return resolved(
                session_id,
                SessionConfidence::High,
                "raw_body.session_id",
                EvidenceLevel::Observed,
                0.98,
            );
        }
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
    use crate::workflow_state::session::{
        MAX_TRACKED_PARENT_SESSIONS, WorkflowIdentityTracker, advance_epoch, resolve_session_signal,
    };

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
