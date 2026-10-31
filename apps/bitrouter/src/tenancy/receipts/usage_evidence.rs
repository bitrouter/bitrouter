//! Original numeric observations, bounded independently of the v1 projection.

use bitrouter_ai::tenancy::{
    execution::ExecutionReceipt,
    identity::{AttemptId, AuthorityRevision},
    usage_evidence::{
        AttemptEvidence, Capture, Dimension, Estimation, ExecutionUsageEvidenceV1, Frame,
        MAX_ATTEMPT_FRAMES, MAX_FRAMES, Protocol, Quantity, Semantics, Source,
    },
};
use serde_json::Value;

pub(in crate::tenancy) mod http;
pub(super) mod storage;
#[cfg(test)]
mod tests;

/// Retained prefix with a sticky stop; terminal metadata has separate capacity.
#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    frames: Vec<Frame>,
    bytes: usize,
    dropped: u64,
    pub(crate) terminals: Vec<AttemptId>,
    pub(crate) estimator_seen: bool,
    pub(crate) estimate: Option<bitrouter_ai::tenancy::usage_evidence::Estimate>,
    pub(crate) parent: Option<bitrouter_ai::tenancy::identity::ExecutionId>,
    pub(crate) children: Vec<bitrouter_ai::tenancy::identity::ExecutionId>,
}
impl Snapshot {
    pub(crate) fn observe(
        &mut self,
        attempt: &AttemptId,
        protocol: Protocol,
        source: Source,
        original: &Value,
    ) {
        // Chat's nonfinal chunks legitimately carry usage:null. These are
        // absence markers, not malformed usage frames or retention pressure.
        if protocol == Protocol::ChatCompletions
            && source == Source::ChatChunk
            && original.is_null()
        {
            return;
        }
        if self.dropped > 0
            || self.frames.len() >= MAX_FRAMES
            || self
                .frames
                .iter()
                .filter(|f| &f.attempt_id == attempt)
                .count()
                >= MAX_ATTEMPT_FRAMES
        {
            self.drop_frame();
            return;
        }
        let mut frame = Frame {
            sequence: self.frames.len() as u64,
            attempt_id: attempt.clone(),
            protocol,
            source,
            semantics: if protocol == Protocol::Decisions {
                Semantics::Unknown
            } else {
                Semantics::Snapshot
            },
            quantities: Vec::new(),
            unsupported: false,
            invalid: false,
        };
        extract(original, mapping(protocol), "", &mut frame);
        let size = match serde_json::to_vec(&frame) {
            Ok(bytes) => bytes.len(),
            Err(_) => {
                self.drop_frame();
                return;
            }
        };
        // Keep 64 KiB for bounded identity, eight targets and terminal metadata.
        if self.bytes.saturating_add(size) > 192 * 1024 {
            self.drop_frame();
            return;
        }
        self.bytes += size;
        self.frames.push(frame);
    }
    fn drop_frame(&mut self) {
        self.dropped = self.dropped.saturating_add(1).min(AuthorityRevision::MAX);
    }
    pub(super) fn document(self, receipt: &ExecutionReceipt) -> ExecutionUsageEvidenceV1 {
        ExecutionUsageEvidenceV1 {
            schema_version: 1,
            installation_id: receipt.installation_id.clone(),
            tenant_id: receipt.tenant_id.clone(),
            execution_id: receipt.execution_id.clone(),
            capture: Capture::Captured,
            extractor_version: 1,
            estimation: if self.estimate.is_some() {
                Estimation::Captured
            } else if self.estimator_seen {
                Estimation::NotUsed
            } else {
                Estimation::NotCaptured
            },
            estimate: self.estimate,
            parent_execution_id: self.parent,
            child_execution_ids: self.children,
            attempts: receipt
                .attempts
                .iter()
                .map(|a| AttemptEvidence {
                    attempt_id: a.attempt_id.clone(),
                    target: a.target.clone(),
                    outcome: a.outcome,
                    terminal_observed: self.terminals.contains(&a.attempt_id)
                        && matches!(
                            a.outcome,
                            bitrouter_ai::tenancy::execution::Outcome::Completed
                                | bitrouter_ai::tenancy::execution::Outcome::Failed
                        ),
                })
                .collect(),
            frames: self.frames,
            truncated: self.dropped > 0,
            dropped_frames: self.dropped,
        }
    }
}

pub(super) fn historical(receipt: &ExecutionReceipt) -> ExecutionUsageEvidenceV1 {
    ExecutionUsageEvidenceV1 {
        schema_version: 1,
        installation_id: receipt.installation_id.clone(),
        tenant_id: receipt.tenant_id.clone(),
        execution_id: receipt.execution_id.clone(),
        capture: Capture::NotCaptured,
        extractor_version: 1,
        estimation: Estimation::NotCaptured,
        estimate: None,
        parent_execution_id: None,
        child_execution_ids: Vec::new(),
        attempts: Vec::new(),
        frames: Vec::new(),
        truncated: false,
        dropped_frames: 0,
    }
}

fn mapping(protocol: Protocol) -> &'static [(&'static str, Dimension)] {
    // Extractor v1: original source fields only. message_delta counters are
    // cumulative snapshots; the event name does not imply additive usage.
    // https://platform.claude.com/docs/en/build-with-claude/streaming
    // https://developers.openai.com/api/reference/resources/chat
    // https://developers.openai.com/api/reference/resources/responses
    use Dimension::*;
    match protocol {
        Protocol::ChatCompletions => &[
            ("prompt_tokens", InputTokens),
            ("completion_tokens", OutputTokens),
            ("total_tokens", TotalTokens),
            ("prompt_tokens_details.cached_tokens", CacheReadTokens),
            ("prompt_tokens_details.audio_tokens", InputAudioTokens),
            ("prompt_tokens_details.image_tokens", InputImageTokens),
            (
                "completion_tokens_details.reasoning_tokens",
                ReasoningTokens,
            ),
            ("completion_tokens_details.audio_tokens", OutputAudioTokens),
            ("completion_tokens_details.image_tokens", OutputImageTokens),
        ],
        Protocol::Responses => &[
            ("input_tokens", InputTokens),
            ("output_tokens", OutputTokens),
            ("total_tokens", TotalTokens),
            ("input_tokens_details.cached_tokens", CacheReadTokens),
            ("input_tokens_details.audio_tokens", InputAudioTokens),
            ("input_tokens_details.image_tokens", InputImageTokens),
            ("output_tokens_details.reasoning_tokens", ReasoningTokens),
            ("output_tokens_details.audio_tokens", OutputAudioTokens),
            ("output_tokens_details.image_tokens", OutputImageTokens),
        ],
        Protocol::Messages => &[
            ("input_tokens", InputTokens),
            ("output_tokens", OutputTokens),
            ("cache_read_input_tokens", CacheReadTokens),
            ("cache_creation_input_tokens", CacheWriteTokens),
            (
                "cache_creation.ephemeral_5m_input_tokens",
                CacheWriteFiveMinuteTokens,
            ),
            (
                "cache_creation.ephemeral_1h_input_tokens",
                CacheWriteOneHourTokens,
            ),
            ("server_tool_use.web_search_requests", WebSearchRequests),
        ],
        Protocol::Decisions => &[
            ("input_tokens", InputTokens),
            ("output_tokens", OutputTokens),
            ("total_tokens", TotalTokens),
        ],
    }
}

fn extract(value: &Value, paths: &[(&str, Dimension)], prefix: &str, frame: &mut Frame) {
    let Some(object) = value.as_object() else {
        frame.invalid = true;
        return;
    };
    // Iterate only known paths for capture. Unknown names are never copied.
    for (path, dimension) in paths {
        if let Some(suffix) = path.strip_prefix(prefix)
            && !suffix.contains('.')
            && let Some(value) = object.get(suffix)
        {
            let number = value.as_u64().filter(|n| *n <= AuthorityRevision::MAX);
            frame.invalid |= number.is_none();
            frame.quantities.push(Quantity {
                dimension: *dimension,
                value: number,
            });
        }
    }
    // Scan at most the allowlist size, regardless of arbitrary source keys.
    let mut known_keys = Vec::with_capacity(paths.len());
    for (path, _) in paths {
        let Some(suffix) = path.strip_prefix(prefix) else {
            continue;
        };
        let Some(head) = suffix.split('.').next() else {
            continue;
        };
        if known_keys.contains(&head) {
            continue;
        }
        known_keys.push(head);
        if suffix.contains('.')
            && let Some(value) = object.get(head)
        {
            extract(value, paths, &path[..prefix.len() + head.len() + 1], frame);
        }
    }
    frame.unsupported |= object.len() > known_keys.len()
        || object
            .keys()
            .take(known_keys.len() + 1)
            .any(|key| !known_keys.contains(&key.as_str()));
}
