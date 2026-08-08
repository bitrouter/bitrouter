//! Responses adapter.
//!
//! Official reference: <https://platform.openai.com/docs/api-reference/responses>
//! Streaming events: <https://platform.openai.com/docs/api-reference/responses-streaming>
//!
//! Responses is its own first-class `ApiProtocol` variant in v1 (v0 only had a
//! conditional branch off Chat Completions). Notable v0 regressions guarded:
//! - #454-3: `input` accepts a plain string **or** an array of items of
//!   several shapes (Codex multi-turn) — parsed leniently, never 400.
//! - #454-2: the streaming envelope is complete, every event carries a
//!   `sequence_number`, `response.completed` carries the full `response`
//!   object, and there is **no** `[DONE]` sentinel.
//! - #432: `response.incomplete` and unknown forward-compatible events are not
//!   mis-flagged as errors.
//! - #434: function-call stream items map `item_id` back to `call_id`, and
//!   argument deltas are not duplicated.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, PROVIDER_ID_OPENAI, SseEvent, StreamDecoder, StreamEncoder,
    Transport, describe_deser_error, provider_defined_native,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, DataContent, FinishReason, GenerateResult, GenerationParams, Message,
    Prompt, ProviderMetadata, ResponseFormat, Role, RoutingTarget, Source, StreamPart, Tool,
    ToolChoice, ToolResultContentPart, ToolResultOutput, Usage, UsageOrigin, provider_namespace,
    set_provider_metadata,
};

const GATEWAY_CONTINUATION_PREFIX: &str = "brc_";
const MAX_GATEWAY_REQUEST_ID_BYTES: usize = 128;
const ASSISTANT_TURN_COMMITMENT_DOMAIN: &[u8] = b"bitrouter.assistant-turn.v1";
const ASSISTANT_TURN_FIELD_DOMAIN: &[u8] = b"bitrouter.assistant-turn.field.v1";
const CAUSAL_PREFIX_ROOT_DOMAIN: &[u8] = b"bitrouter.causal-prefix.root.v1";
const CAUSAL_PREFIX_STEP_DOMAIN: &[u8] = b"bitrouter.causal-prefix.step.v1";
const MAX_STREAMING_COMMITMENT_TOOL_CALLS: usize = 64;

/// Fixed-size, protocol-neutral commitment to one canonical assistant turn.
/// Provider metadata and stream fragmentation are intentionally excluded.
#[derive(Clone, PartialEq, Eq)]
pub struct AssistantTurnCommitment {
    encoded: String,
    digest: [u8; 32],
}

impl AssistantTurnCommitment {
    /// Parse the bounded canonical storage representation.
    pub fn parse(value: &str) -> Option<Self> {
        Some(Self {
            encoded: value.to_owned(),
            digest: decode_digest(value)?,
        })
    }

    /// Borrow the bounded canonical storage representation.
    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    fn from_digest(digest: [u8; 32]) -> Self {
        Self {
            encoded: format!("sha256:{}", encode_digest(digest)),
            digest,
        }
    }
}

impl std::fmt::Debug for AssistantTurnCommitment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AssistantTurnCommitment(<redacted>)")
    }
}

/// Fixed-size rolling commitment to one complete causal message prefix.
/// The encrypted continuation row supplies owner and gateway identity binding.
#[derive(Clone, PartialEq, Eq)]
pub struct CausalPrefixCommitment {
    encoded: String,
    digest: [u8; 32],
}

impl CausalPrefixCommitment {
    /// Parse the bounded canonical storage representation.
    pub fn parse(value: &str) -> Option<Self> {
        Some(Self {
            encoded: value.to_owned(),
            digest: decode_digest(value)?,
        })
    }

    /// Borrow the bounded canonical storage representation.
    pub fn as_str(&self) -> &str {
        &self.encoded
    }

    fn from_digest(digest: [u8; 32]) -> Self {
        Self {
            encoded: format!("sha256:{}", encode_digest(digest)),
            digest,
        }
    }
}

impl std::fmt::Debug for CausalPrefixCommitment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CausalPrefixCommitment(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FieldDigest {
    len: u64,
    digest: [u8; 32],
}

impl FieldDigest {
    fn from_bytes(value: &[u8]) -> Option<Self> {
        let mut hasher = Sha256::new();
        hasher.update(ASSISTANT_TURN_FIELD_DOMAIN);
        hasher.update(value);
        Some(Self {
            len: u64::try_from(value.len()).ok()?,
            digest: hasher.finalize().into(),
        })
    }
}

struct FieldAccumulator {
    hasher: Sha256,
    len: u64,
}

impl Default for FieldAccumulator {
    fn default() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(ASSISTANT_TURN_FIELD_DOMAIN);
        Self { hasher, len: 0 }
    }
}

impl FieldAccumulator {
    fn update(&mut self, value: &[u8]) -> bool {
        let Ok(len) = u64::try_from(value.len()) else {
            return false;
        };
        let Some(total) = self.len.checked_add(len) else {
            return false;
        };
        self.hasher.update(value);
        self.len = total;
        true
    }

    fn finish(self) -> FieldDigest {
        FieldDigest {
            len: self.len,
            digest: self.hasher.finalize().into(),
        }
    }
}

struct TurnAccumulator {
    hasher: Sha256,
    part_count: u64,
    meaningful: bool,
    valid: bool,
}

impl Default for TurnAccumulator {
    fn default() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(ASSISTANT_TURN_COMMITMENT_DOMAIN);
        Self {
            hasher,
            part_count: 0,
            meaningful: false,
            valid: true,
        }
    }
}

impl TurnAccumulator {
    fn add_part(&mut self, tag: u8, fields: &[FieldDigest], meaningful: bool) {
        if !self.valid {
            return;
        }
        let Some(part_count) = self.part_count.checked_add(1) else {
            self.valid = false;
            return;
        };
        self.hasher.update([tag]);
        self.hasher.update((fields.len() as u64).to_be_bytes());
        for field in fields {
            self.hasher.update(field.len.to_be_bytes());
            self.hasher.update(field.digest);
        }
        self.part_count = part_count;
        self.meaningful |= meaningful;
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }

    fn finish(mut self) -> Option<AssistantTurnCommitment> {
        if !self.valid || !self.meaningful || self.part_count == 0 {
            return None;
        }
        self.hasher.update(self.part_count.to_be_bytes());
        Some(AssistantTurnCommitment::from_digest(
            self.hasher.finalize().into(),
        ))
    }
}

fn encode_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    let encoded = value.strip_prefix("sha256:")?.as_bytes();
    if encoded.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in encoded.chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn field(value: &str) -> Option<FieldDigest> {
    FieldDigest::from_bytes(value.as_bytes())
}

fn bool_field(value: bool) -> FieldDigest {
    let value = [u8::from(value)];
    let mut hasher = Sha256::new();
    hasher.update(ASSISTANT_TURN_FIELD_DOMAIN);
    hasher.update(value);
    FieldDigest {
        len: 1,
        digest: hasher.finalize().into(),
    }
}

fn observe_tool_call(
    turn: &mut TurnAccumulator,
    id: &str,
    name: &str,
    arguments: &str,
    provider_executed: bool,
    dynamic: bool,
) {
    let (Some(id), Some(name), Some(arguments)) = (field(id), field(name), field(arguments)) else {
        turn.invalidate();
        return;
    };
    if id.len == 0 || name.len == 0 {
        turn.invalidate();
        return;
    }
    turn.add_part(
        3,
        &[
            id,
            name,
            arguments,
            bool_field(provider_executed),
            bool_field(dynamic),
        ],
        true,
    );
}

fn observe_content(turn: &mut TurnAccumulator, content: &Content) {
    let mut fields = Vec::with_capacity(8);
    let (tag, meaningful) = match content {
        Content::Text { text, .. } => {
            let Some(value) = field(text) else {
                turn.invalidate();
                return;
            };
            fields.push(value);
            (1, !text.is_empty())
        }
        Content::Reasoning { text, .. } => {
            let Some(value) = field(text) else {
                turn.invalidate();
                return;
            };
            fields.push(value);
            (2, !text.is_empty())
        }
        Content::ToolCall {
            id,
            name,
            arguments,
            provider_executed,
            dynamic,
            ..
        } => {
            observe_tool_call(turn, id, name, arguments, *provider_executed, *dynamic);
            return;
        }
        Content::File { .. }
        | Content::ToolResult { .. }
        | Content::Source { .. }
        | Content::ToolApprovalRequest { .. }
        | Content::ToolApprovalResponse { .. } => {
            turn.invalidate();
            return;
        }
    };
    turn.add_part(tag, &fields, meaningful);
}

/// Compute the same commitment used by streaming finalization without cloning
/// or retaining the canonical response content.
pub fn assistant_turn_commitment(content: &[Content]) -> Option<AssistantTurnCommitment> {
    let mut turn = TurnAccumulator::default();
    for part in content {
        observe_content(&mut turn, part);
    }
    turn.finish()
}

/// Hash the complete terminal Responses output without materializing another
/// transcript. Only protocol-neutral assistant semantics supported by the
/// post-hook stream accumulator are eligible; every other item fails closed.
fn terminal_assistant_turn_commitment(
    response: &serde_json::Value,
) -> Option<AssistantTurnCommitment> {
    let output = response.get("output")?.as_array()?;
    if output.is_empty() {
        return None;
    }
    let mut turn = TurnAccumulator::default();
    for item in output {
        match item.get("type")?.as_str()? {
            "message" => {
                if item.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                    return None;
                }
                let content = item.get("content")?.as_array()?;
                if content.is_empty() {
                    return None;
                }
                for part in content {
                    if !matches!(
                        part.get("type").and_then(serde_json::Value::as_str),
                        Some("output_text" | "text")
                    ) || part.get("annotations").is_some_and(|annotations| {
                        !annotations.as_array().is_some_and(Vec::is_empty)
                    }) {
                        return None;
                    }
                    let text = part.get("text")?.as_str()?;
                    let value = field(text)?;
                    turn.add_part(1, std::slice::from_ref(&value), !text.is_empty());
                }
            }
            "reasoning" => {
                let summary = item.get("summary")?.as_array()?;
                if summary.is_empty() {
                    return None;
                }
                let mut value = FieldAccumulator::default();
                for part in summary {
                    if part.get("type").and_then(serde_json::Value::as_str) != Some("summary_text")
                    {
                        return None;
                    }
                    if !value.update(part.get("text")?.as_str()?.as_bytes()) {
                        return None;
                    }
                }
                let value = value.finish();
                turn.add_part(2, std::slice::from_ref(&value), value.len > 0);
            }
            "function_call" => observe_tool_call(
                &mut turn,
                item.get("call_id")?.as_str()?,
                item.get("name")?.as_str()?,
                item.get("arguments")?.as_str()?,
                false,
                false,
            ),
            _ => return None,
        }
    }
    turn.finish()
}

fn causal_content_role(message_role: Role, content: &Content) -> Option<Role> {
    match content {
        Content::Text { .. } => Some(message_role),
        Content::Reasoning { .. } | Content::ToolCall { .. } => Some(Role::Assistant),
        Content::ToolResult { .. } => Some(Role::Tool),
        Content::File { .. }
        | Content::Source { .. }
        | Content::ToolApprovalRequest { .. }
        | Content::ToolApprovalResponse { .. } => None,
    }
}

fn causal_message_role(message: &Message) -> Option<Role> {
    if message.role == Role::System {
        return Some(Role::System);
    }
    let first = message.content.first()?;
    let role = causal_content_role(message.role, first)?;
    if message
        .content
        .iter()
        .any(|content| causal_content_role(message.role, content) != Some(role))
    {
        return None;
    }
    Some(role)
}

fn observe_causal_content(turn: &mut TurnAccumulator, content: &Content) {
    match content {
        Content::Text { .. } | Content::Reasoning { .. } | Content::ToolCall { .. } => {
            observe_content(turn, content);
        }
        Content::ToolResult {
            call_id,
            output,
            dynamic,
            ..
        } => {
            let (output_tag, output_value) = match output {
                ToolResultOutput::Text { value } => ("text", value.as_str()),
                ToolResultOutput::ErrorText { value } => ("error_text", value.as_str()),
                ToolResultOutput::ExecutionDenied { reason } => {
                    ("execution_denied", reason.as_deref().unwrap_or(""))
                }
                ToolResultOutput::Json { .. }
                | ToolResultOutput::ErrorJson { .. }
                | ToolResultOutput::Content { .. } => {
                    turn.invalidate();
                    return;
                }
            };
            let (Some(call_id), Some(output_tag), Some(output_value)) =
                (field(call_id), field(output_tag), field(output_value))
            else {
                turn.invalidate();
                return;
            };
            if call_id.len == 0 {
                turn.invalidate();
                return;
            }
            turn.add_part(
                4,
                &[call_id, output_tag, output_value, bool_field(*dynamic)],
                true,
            );
        }
        Content::File { .. }
        | Content::Source { .. }
        | Content::ToolApprovalRequest { .. }
        | Content::ToolApprovalResponse { .. } => turn.invalidate(),
    }
}

fn initial_causal_prefix() -> CausalPrefixCommitment {
    CausalPrefixCommitment::from_digest(Sha256::digest(CAUSAL_PREFIX_ROOT_DOMAIN).into())
}

fn extend_causal_step(
    prior: &CausalPrefixCommitment,
    role: Role,
    message: &AssistantTurnCommitment,
) -> CausalPrefixCommitment {
    let mut hasher = Sha256::new();
    hasher.update(CAUSAL_PREFIX_STEP_DOMAIN);
    hasher.update(prior.digest);
    hasher.update([match role {
        Role::System => 0,
        Role::User => 1,
        Role::Assistant => 2,
        Role::Tool => 3,
    }]);
    hasher.update(message.digest);
    CausalPrefixCommitment::from_digest(hasher.finalize().into())
}

fn fold_causal_messages(
    prior: Option<&CausalPrefixCommitment>,
    messages: &[Message],
    mut observe: impl FnMut(usize, Role, &CausalPrefixCommitment),
) -> Option<CausalPrefixCommitment> {
    let mut root = prior.cloned().unwrap_or_else(initial_causal_prefix);
    let mut index = 0;
    while index < messages.len() {
        let role = causal_message_role(&messages[index])?;
        if role == Role::System {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        let mut turn = TurnAccumulator::default();
        for content in &messages[index].content {
            observe_causal_content(&mut turn, content);
        }
        if matches!(role, Role::Assistant | Role::Tool) {
            while end < messages.len() && causal_message_role(&messages[end]) == Some(role) {
                for content in &messages[end].content {
                    observe_causal_content(&mut turn, content);
                }
                end += 1;
            }
        }
        let message = turn.finish()?;
        root = extend_causal_step(&root, role, &message);
        observe(end, role, &root);
        index = end;
    }
    Some(root)
}

/// Extend an authenticated prior causal root with the current visible suffix
/// and the newly delivered assistant turn. System/developer envelope content is
/// excluded so clients that resend it do not change the conversation chain.
pub fn extend_causal_prefix(
    prior: Option<&CausalPrefixCommitment>,
    messages: &[Message],
    delivered: &AssistantTurnCommitment,
) -> Option<CausalPrefixCommitment> {
    let root = fold_causal_messages(prior, messages, |_, _, _| {})?;
    Some(extend_causal_step(&root, Role::Assistant, delivered))
}

/// Find the unique exact prior root in a complete visible history. The match
/// must end at the last assistant turn; truncated and repeated-parent suffixes
/// therefore fail closed.
pub fn find_unique_causal_prefix(
    messages: &[Message],
    expected: &CausalPrefixCommitment,
) -> Option<usize> {
    let mut matching_end = None;
    let mut ambiguous = false;
    let mut last_assistant_end = None;
    fold_causal_messages(None, messages, |end, role, root| {
        if role == Role::Assistant {
            last_assistant_end = Some(end);
            if root == expected && matching_end.replace(end).is_some() {
                ambiguous = true;
            }
        }
    })?;
    let matching_end = matching_end?;
    (!ambiguous && last_assistant_end == Some(matching_end)).then_some(matching_end)
}

/// Request-local instructions for extending a continuation's authenticated
/// causal root during required finalization.
#[derive(Clone, Debug)]
pub struct CausalPrefixPlan {
    prior: Option<CausalPrefixCommitment>,
    suffix_start: Option<usize>,
}

impl CausalPrefixPlan {
    /// Extend `prior` with `messages[suffix_start..]` and the delivered turn.
    pub fn extend(prior: CausalPrefixCommitment, suffix_start: usize) -> Self {
        Self {
            prior: Some(prior),
            suffix_start: Some(suffix_start),
        }
    }

    /// Prevent publication of a new root when ancestry cannot be reconstructed.
    pub fn ineligible() -> Self {
        Self {
            prior: None,
            suffix_start: None,
        }
    }

    pub(crate) fn finalize(
        &self,
        messages: &[Message],
        delivered: &AssistantTurnCommitment,
    ) -> Option<CausalPrefixCommitment> {
        let start = self.suffix_start?;
        let suffix = messages.get(start..)?;
        extend_causal_prefix(self.prior.as_ref(), suffix, delivered)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamingFieldKind {
    Text,
    Reasoning,
}

struct ActiveStreamingField {
    kind: StreamingFieldKind,
    marker: Option<FieldDigest>,
    value: FieldAccumulator,
}

struct StreamingToolCall {
    id: FieldDigest,
    name: Option<FieldDigest>,
    arguments: FieldAccumulator,
}

/// Fixed-size incremental commitment builder for streamed assistant output.
/// It never retains text, reasoning, arguments, files, or tool results.
pub struct StreamingAssistantTurnCommitment {
    turn: TurnAccumulator,
    active: Option<ActiveStreamingField>,
    tools: [Option<StreamingToolCall>; MAX_STREAMING_COMMITMENT_TOOL_CALLS],
    tool_count: usize,
    tool_batch_started: bool,
}

impl Default for StreamingAssistantTurnCommitment {
    fn default() -> Self {
        Self {
            turn: TurnAccumulator::default(),
            active: None,
            tools: std::array::from_fn(|_| None),
            tool_count: 0,
            tool_batch_started: false,
        }
    }
}

impl StreamingAssistantTurnCommitment {
    fn finish_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let value = active.value.finish();
        let tag = match active.kind {
            StreamingFieldKind::Text => 1,
            StreamingFieldKind::Reasoning => 2,
        };
        self.turn
            .add_part(tag, std::slice::from_ref(&value), value.len > 0);
    }

    fn begin_field(&mut self, kind: StreamingFieldKind, marker: Option<&str>) {
        if self.tool_batch_started {
            self.turn.invalidate();
            return;
        }
        self.finish_active();
        self.active = Some(ActiveStreamingField {
            kind,
            marker: marker.and_then(field),
            value: FieldAccumulator::default(),
        });
    }

    fn update_field(&mut self, kind: StreamingFieldKind, value: &str) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.kind != kind)
        {
            self.begin_field(kind, None);
        }
        if let Some(active) = self.active.as_mut()
            && !active.value.update(value.as_bytes())
        {
            self.turn.invalidate();
        }
    }

    fn end_field(&mut self, kind: StreamingFieldKind, marker: &str) {
        let marker = field(marker);
        if self.active.as_ref().is_none_or(|active| {
            active.kind != kind || (active.marker.is_some() && active.marker != marker)
        }) {
            self.turn.invalidate();
            return;
        }
        self.finish_active();
    }

    fn observe_tool_delta(&mut self, id: &str, name: Option<&str>, arguments: &str) {
        self.finish_active();
        self.tool_batch_started = true;
        let Some(id_digest) = field(id) else {
            self.turn.invalidate();
            return;
        };
        if id_digest.len == 0 {
            self.turn.invalidate();
            return;
        }
        let index = self.tools[..self.tool_count]
            .iter()
            .position(|tool| tool.as_ref().is_some_and(|tool| tool.id == id_digest));
        let index = match (index, name) {
            (Some(index), None) => index,
            (Some(_), Some(_)) | (None, None) => {
                self.turn.invalidate();
                return;
            }
            (None, Some(_)) if self.tool_count < MAX_STREAMING_COMMITMENT_TOOL_CALLS => {
                let index = self.tool_count;
                self.tools[index] = Some(StreamingToolCall {
                    id: id_digest,
                    name: None,
                    arguments: FieldAccumulator::default(),
                });
                self.tool_count += 1;
                index
            }
            (None, Some(_)) => {
                self.turn.invalidate();
                return;
            }
        };
        let Some(tool) = self.tools[index].as_mut() else {
            self.turn.invalidate();
            return;
        };
        if let Some(name) = name {
            let Some(name) = field(name) else {
                self.turn.invalidate();
                return;
            };
            if name.len == 0 || tool.name.as_ref().is_some_and(|existing| existing != &name) {
                self.turn.invalidate();
                return;
            }
            tool.name = Some(name);
        }
        if !tool.arguments.update(arguments.as_bytes()) {
            self.turn.invalidate();
        }
    }

    /// Observe one post-hook downstream part.
    pub fn observe(&mut self, part: &StreamPart) {
        match part {
            StreamPart::TextStart { id } => {
                self.begin_field(StreamingFieldKind::Text, Some(id));
            }
            StreamPart::TextDelta { text } => {
                self.update_field(StreamingFieldKind::Text, text);
            }
            StreamPart::TextEnd { id } => self.end_field(StreamingFieldKind::Text, id),
            StreamPart::ReasoningStart { id } => {
                self.begin_field(StreamingFieldKind::Reasoning, Some(id));
            }
            StreamPart::ReasoningDelta { text } => {
                self.update_field(StreamingFieldKind::Reasoning, text);
            }
            StreamPart::ReasoningEnd { id, .. } => {
                self.end_field(StreamingFieldKind::Reasoning, id);
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => self.observe_tool_delta(id, name.as_deref(), arguments),
            StreamPart::ServerToolCall {
                id,
                name,
                arguments,
                dynamic,
                ..
            } => {
                self.finish_active();
                if self.tool_batch_started {
                    self.turn.invalidate();
                    return;
                }
                observe_tool_call(&mut self.turn, id, name, arguments, true, *dynamic);
            }
            StreamPart::ServerToolResult { .. }
            | StreamPart::File { .. }
            | StreamPart::Source { .. } => {
                self.finish_active();
                self.turn.invalidate();
            }
            StreamPart::Usage { .. }
            | StreamPart::ResponseStarted { .. }
            | StreamPart::Finish { .. }
            | StreamPart::ResponseCompleted { .. } => {}
        }
    }

    /// Finalize the bounded commitment. Unsupported or ambiguous stream shapes
    /// return `None`, forcing continuation pinning rather than detach.
    pub fn finish(mut self) -> Option<AssistantTurnCommitment> {
        self.finish_active();
        for index in 0..self.tool_count {
            let Some(tool) = self.tools[index].take() else {
                self.turn.invalidate();
                continue;
            };
            let Some(name) = tool.name else {
                self.turn.invalidate();
                continue;
            };
            let arguments = tool.arguments.finish();
            self.turn.add_part(
                3,
                &[
                    tool.id,
                    name,
                    arguments,
                    bool_field(false),
                    bool_field(false),
                ],
                true,
            );
        }
        self.turn.finish()
    }
}

fn created_at_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Encode the existing gateway request id into BitRouter's reserved public
/// Responses continuation namespace.
pub fn encode_gateway_continuation_id(request_id: &str) -> Result<String> {
    validate_gateway_request_id(request_id)?;
    Ok(format!(
        "{GATEWAY_CONTINUATION_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(request_id.as_bytes())
    ))
}

/// Strictly decode a BitRouter public continuation id. Non-reserved provider
/// ids return `None`; malformed reserved ids fail closed.
pub fn decode_gateway_continuation_id(value: &str) -> Result<Option<String>> {
    let Some(payload) = value.strip_prefix(GATEWAY_CONTINUATION_PREFIX) else {
        return Ok(None);
    };
    if payload.is_empty() {
        return Err(BitrouterError::bad_request(
            "gateway continuation id has an empty payload",
        ));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| BitrouterError::bad_request("gateway continuation id is malformed"))?;
    let request_id = String::from_utf8(decoded)
        .map_err(|_| BitrouterError::bad_request("gateway continuation id is not UTF-8"))?;
    validate_gateway_request_id(&request_id)?;
    let canonical = encode_gateway_continuation_id(&request_id)?;
    if canonical != value {
        return Err(BitrouterError::bad_request(
            "gateway continuation id is not canonical",
        ));
    }
    Ok(Some(request_id))
}

fn validate_gateway_request_id(request_id: &str) -> Result<()> {
    if request_id.is_empty() {
        return Err(BitrouterError::bad_request("request id must not be empty"));
    }
    if request_id.len() > MAX_GATEWAY_REQUEST_ID_BYTES {
        return Err(BitrouterError::bad_request(format!(
            "request id exceeds {MAX_GATEWAY_REQUEST_ID_BYTES} bytes"
        )));
    }
    if request_id.chars().any(char::is_control) {
        return Err(BitrouterError::bad_request(
            "request id must not contain control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod assistant_turn_commitment_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn text(value: &str) -> Content {
        Content::Text {
            text: value.to_owned(),
            provider_metadata: Default::default(),
        }
    }

    fn client_tool_call(id: &str, name: &str, arguments: &str) -> Content {
        Content::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }
    }

    #[test]
    fn commitment_is_shared_by_nonstream_history_and_fragmented_streams() {
        let content = vec![text("parent response")];
        let expected = assistant_turn_commitment(&content);
        let mut streamed = StreamingAssistantTurnCommitment::default();
        streamed.observe(&StreamPart::TextStart {
            id: "item-1".into(),
        });
        streamed.observe(&StreamPart::TextDelta {
            text: "parent ".into(),
        });
        streamed.observe(&StreamPart::TextDelta {
            text: "response".into(),
        });
        streamed.observe(&StreamPart::TextEnd {
            id: "item-1".into(),
        });

        assert!(expected.is_some());
        assert_eq!(streamed.finish(), expected);
    }

    #[test]
    fn commitment_ignores_provider_metadata_but_rejects_empty_semantics() {
        let baseline = vec![text("visible")];
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "openai".to_owned(),
            serde_json::json!({"itemId": "provider-private"}),
        );
        let decorated = vec![Content::Text {
            text: "visible".to_owned(),
            provider_metadata: metadata,
        }];

        assert_eq!(
            assistant_turn_commitment(&baseline),
            assistant_turn_commitment(&decorated)
        );
        assert_eq!(assistant_turn_commitment(&[]), None);
        assert_eq!(assistant_turn_commitment(&[text("")]), None);
    }

    #[test]
    fn unsupported_content_fails_closed_in_nonstream_and_streaming_paths() {
        let unsupported = Content::Source {
            source: Source::Url {
                id: "source-1".into(),
                url: "https://example.test/evidence".into(),
                title: Some("evidence".into()),
            },
            provider_metadata: Default::default(),
        };
        assert_eq!(
            assistant_turn_commitment(&[text("answer"), unsupported]),
            None
        );

        let mut streamed = StreamingAssistantTurnCommitment::default();
        streamed.observe(&StreamPart::TextDelta {
            text: "answer".into(),
        });
        streamed.observe(&StreamPart::Source {
            source: Source::Url {
                id: "source-1".into(),
                url: "https://example.test/evidence".into(),
                title: Some("evidence".into()),
            },
        });
        assert_eq!(streamed.finish(), None);
    }

    #[test]
    fn terminal_output_is_authoritative_and_rejects_silently_dropped_items() {
        let supported = serde_json::json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer", "annotations": []}]
            }]
        });
        assert_eq!(
            terminal_assistant_turn_commitment(&supported),
            assistant_turn_commitment(&[text("answer")])
        );

        for unsupported in [
            serde_json::json!({"output": [{"type": "mcp_call", "id": "mcp-1"}]}),
            serde_json::json!({"output": [{"type": "mcp_approval_request", "id": "approval-1"}]}),
            serde_json::json!({"output": [{"type": "web_search_call", "id": "search-1"}]}),
            serde_json::json!({
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "answer",
                        "annotations": [{"type": "url_citation", "url": "https://example.test"}]
                    }]
                }]
            }),
        ] {
            assert_eq!(terminal_assistant_turn_commitment(&unsupported), None);
        }
    }

    #[test]
    fn interleaved_tool_call_fragments_match_complete_content() {
        let expected = assistant_turn_commitment(&[
            client_tool_call("call-a", "inspect", r#"{"path":"src/"}"#),
            client_tool_call("call-b", "search", r#"{"query":"TODO"}"#),
        ]);
        let mut streamed = StreamingAssistantTurnCommitment::default();
        for part in [
            StreamPart::ToolCallDelta {
                id: "call-a".into(),
                name: Some("inspect".into()),
                arguments: "{\"path\":\"".into(),
            },
            StreamPart::ToolCallDelta {
                id: "call-b".into(),
                name: Some("search".into()),
                arguments: "{\"query\":\"".into(),
            },
            StreamPart::ToolCallDelta {
                id: "call-a".into(),
                name: None,
                arguments: "src/\"}".into(),
            },
            StreamPart::ToolCallDelta {
                id: "call-b".into(),
                name: None,
                arguments: "TODO\"}".into(),
            },
        ] {
            streamed.observe(&part);
        }

        assert!(expected.is_some());
        assert_eq!(streamed.finish(), expected);
    }

    #[test]
    fn duplicate_name_bearing_tool_call_id_fails_closed() {
        let mut streamed = StreamingAssistantTurnCommitment::default();
        for part in [
            StreamPart::ToolCallDelta {
                id: "call-1".into(),
                name: Some("inspect".into()),
                arguments: "{".into(),
            },
            StreamPart::ToolCallDelta {
                id: "call-1".into(),
                name: Some("inspect".into()),
                arguments: "}".into(),
            },
        ] {
            streamed.observe(&part);
        }

        assert_eq!(streamed.finish(), None);
    }

    #[test]
    fn streaming_tool_call_capacity_is_exactly_sixty_four() {
        let mut at_limit = StreamingAssistantTurnCommitment::default();
        for index in 0..64 {
            at_limit.observe(&StreamPart::ToolCallDelta {
                id: format!("call-{index}"),
                name: Some(format!("tool-{index}")),
                arguments: "{}".into(),
            });
        }
        assert!(at_limit.finish().is_some());

        let mut over_limit = StreamingAssistantTurnCommitment::default();
        for index in 0..65 {
            over_limit.observe(&StreamPart::ToolCallDelta {
                id: format!("call-{index}"),
                name: Some(format!("tool-{index}")),
                arguments: "{}".into(),
            });
        }
        assert_eq!(over_limit.finish(), None);
    }

    #[test]
    fn large_fragmented_stream_has_a_fixed_size_accumulator() {
        const FRAGMENT: &str = "0123456789abcdef0123456789abcdef";
        const REPEATS: usize = 262_144;
        let full = FRAGMENT.repeat(REPEATS);
        let expected = assistant_turn_commitment(&[text(&full)]);
        let mut streamed = StreamingAssistantTurnCommitment::default();
        streamed.observe(&StreamPart::TextStart { id: "large".into() });
        for _ in 0..REPEATS {
            streamed.observe(&StreamPart::TextDelta {
                text: FRAGMENT.into(),
            });
        }
        streamed.observe(&StreamPart::TextEnd { id: "large".into() });

        assert!(std::mem::size_of::<StreamingAssistantTurnCommitment>() <= 32 * 1024);
        assert_eq!(streamed.finish(), expected);
    }

    #[test]
    fn rolling_causal_prefix_binds_the_complete_visible_history() {
        let opening = Message::text(Role::User, "opening request");
        let parent = Message::text(Role::Assistant, "parent response");
        let parent_output = assistant_turn_commitment(&parent.content).expect("parent commitment");
        let root = extend_causal_prefix(None, std::slice::from_ref(&opening), &parent_output)
            .expect("root commitment");
        let followup = Message::text(Role::User, "follow up");
        let visible = vec![opening.clone(), parent.clone(), followup.clone()];

        assert_eq!(find_unique_causal_prefix(&visible, &root), Some(2));

        let changed = vec![
            Message::text(Role::User, "different opening"),
            parent.clone(),
            followup.clone(),
        ];
        let sliding = vec![parent.clone(), followup.clone()];
        let repeated = vec![
            opening,
            parent.clone(),
            Message::text(Role::User, "middle"),
            parent,
            followup,
        ];
        for rejected in [changed, sliding, repeated] {
            assert_eq!(find_unique_causal_prefix(&rejected, &root), None);
        }
    }

    #[test]
    fn assistant_output_item_fragmentation_keeps_one_causal_turn() {
        let opening = Message::text(Role::User, "opening request");
        let parent_content = vec![
            text("I will inspect it."),
            client_tool_call("call-1", "inspect", r#"{"path":"src"}"#),
        ];
        let delivered = assistant_turn_commitment(&parent_content).expect("parent commitment");
        let root = extend_causal_prefix(None, std::slice::from_ref(&opening), &delivered)
            .expect("root commitment");
        let visible = vec![
            opening,
            Message::text(Role::Assistant, "I will inspect it."),
            Message {
                role: Role::Assistant,
                content: vec![client_tool_call("call-1", "inspect", r#"{"path":"src"}"#)],
            },
            Message::text(Role::User, "continue"),
        ];

        assert_eq!(find_unique_causal_prefix(&visible, &root), Some(3));
    }

    #[test]
    fn hidden_suffix_extension_matches_the_full_visible_chain() {
        let opening = Message::text(Role::User, "opening request");
        let parent = Message::text(Role::Assistant, "parent response");
        let parent_output = assistant_turn_commitment(&parent.content).expect("parent commitment");
        let root = extend_causal_prefix(None, std::slice::from_ref(&opening), &parent_output)
            .expect("root commitment");
        let followup = Message::text(Role::User, "hidden follow up");
        let second = Message::text(Role::Assistant, "second response");
        let second_output = assistant_turn_commitment(&second.content).expect("second commitment");

        let extended =
            extend_causal_prefix(Some(&root), std::slice::from_ref(&followup), &second_output)
                .expect("extended commitment");
        let rebuilt = extend_causal_prefix(None, &[opening, parent, followup], &second_output)
            .expect("rebuilt commitment");

        assert_eq!(extended, rebuilt);
    }

    #[test]
    fn repeated_system_envelope_does_not_change_the_causal_root() {
        let system = Message::text(Role::System, "stable instructions");
        let opening = Message::text(Role::User, "opening request");
        let parent = Message::text(Role::Assistant, "parent response");
        let parent_output = assistant_turn_commitment(&parent.content).expect("parent commitment");
        let root = extend_causal_prefix(None, &[system.clone(), opening.clone()], &parent_output)
            .expect("root commitment");
        let second_output =
            assistant_turn_commitment(&[text("second response")]).expect("second commitment");

        let hidden = extend_causal_prefix(
            Some(&root),
            &[system.clone(), Message::text(Role::User, "follow up")],
            &second_output,
        )
        .expect("hidden root");
        let visible = extend_causal_prefix(
            None,
            &[
                system,
                opening,
                parent,
                Message::text(Role::User, "follow up"),
            ],
            &second_output,
        )
        .expect("visible root");

        assert_eq!(hidden, visible);
    }
}

/// Synthesize a stable `tool_call_id` for a [`Content::ToolApprovalRequest`]
/// parsed from a Responses `mcp_approval_request`. That item transmits an
/// `approval_request_id` but no separate tool-call id, and V3's
/// `ToolApprovalRequest` requires one (the AI SDK generates a fresh id here);
/// deriving it deterministically from the approval id (`approval:<id>`) keeps a
/// same-protocol round-trip stable without fabricating provider-side identity.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-approval-request.ts>
fn synthesize_approval_tool_call_id(approval_id: &str) -> String {
    format!("approval:{approval_id}")
}

/// The tool-name prefix the AI SDK gives a remote MCP tool surfaced from an
/// `mcp_call` item (`mcp.<name>`). bitrouter keeps the same prefix so the tool
/// name round-trips and the render can recover the bare MCP tool name.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
const RESPONSES_MCP_TOOL_PREFIX: &str = "mcp.";
/// The synthetic tool name for an OpenAI Responses `local_shell_call` — a
/// client-executed shell tool keyed by `call_id`. Matches the AI SDK's custom
/// tool name so the input `action` payload round-trips.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/tool/local-shell.ts>
const RESPONSES_LOCAL_SHELL_TOOL: &str = "local_shell";
/// The `provider_metadata["openai"]` key holding a Responses output item's `id`,
/// distinct from its `call_id`. Restored on render so a same-protocol round-trip
/// reproduces the original item id (mirrors the AI SDK `itemId` key).
/// <https://platform.openai.com/docs/api-reference/responses/object>
const RESPONSES_ITEM_ID: &str = "itemId";
/// The discriminator the AI SDK stamps on an `mcp_call`'s lowered tool-result
/// body (`{ type: 'call', serverLabel, name, arguments, output?, error? }`). The
/// Responses encoder recognises it to recombine a dynamic `ToolCall` + its
/// same-id `ToolResult` back into a single `mcp_call` item.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/tool/mcp.ts>
const RESPONSES_MCP_CALL_TAG: &str = "call";

/// Lower an OpenAI Responses `mcp_call` item — a provider-executed remote MCP
/// tool call whose result is carried **inline** — into the canonical pair the
/// AI SDK reference produces: a `dynamic`, provider-executed [`Content::ToolCall`]
/// plus a paired [`Content::ToolResult`] whose body is the MCP-specific
/// `{ type: 'call', serverLabel, name, arguments, output?, error? }` JSON object.
/// Keeping the result as that exact structure (rather than splitting `output`
/// onto a bare string) lets [`render_output_items`] recombine the two parts into
/// one `mcp_call` item, so the inline result round-trips same-protocol exactly.
/// The item `id` correlates the pair and is preserved under
/// `provider_metadata["openai"]["itemId"]`.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
fn parse_mcp_call(item: &serde_json::Value) -> Vec<Content> {
    let id = item
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    let arguments = item
        .get("arguments")
        .and_then(|a| a.as_str())
        .unwrap_or_default()
        .to_string();
    // Build the MCP result body, mirroring the AI SDK `mcpOutputSchema`:
    // `{type:'call', serverLabel, name, arguments, output?, error?}`. `output`
    // and `error` are only present when the wire carried them.
    let mut result = serde_json::Map::new();
    result.insert("type".into(), RESPONSES_MCP_CALL_TAG.into());
    if let Some(label) = item.get("server_label") {
        result.insert("serverLabel".into(), label.clone());
    }
    result.insert("name".into(), name.clone().into());
    result.insert("arguments".into(), arguments.clone().into());
    if let Some(output) = item.get("output").filter(|v| !v.is_null()) {
        result.insert("output".into(), output.clone());
    }
    if let Some(error) = item.get("error").filter(|v| !v.is_null()) {
        result.insert("error".into(), error.clone());
    }
    let mut meta = ProviderMetadata::new();
    set_provider_metadata(
        &mut meta,
        PROVIDER_ID_OPENAI,
        RESPONSES_ITEM_ID,
        serde_json::Value::String(id.clone()),
    );
    vec![
        Content::ToolCall {
            id: id.clone(),
            name: format!("{RESPONSES_MCP_TOOL_PREFIX}{name}"),
            arguments,
            provider_executed: true,
            dynamic: true,
            provider_metadata: ProviderMetadata::new(),
        },
        Content::ToolResult {
            call_id: id,
            tool_name: Some(format!("{RESPONSES_MCP_TOOL_PREFIX}{name}")),
            output: ToolResultOutput::Json {
                value: serde_json::Value::Object(result),
            },
            dynamic: true,
            provider_metadata: meta,
        },
    ]
}

/// The OpenAI `itemId` preserved in `provider_metadata`, if any.
fn responses_item_id(meta: &ProviderMetadata) -> Option<String> {
    provider_namespace(meta, PROVIDER_ID_OPENAI)
        .and_then(|o| o.get(RESPONSES_ITEM_ID))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Recombine a `dynamic` provider-executed MCP [`Content::ToolCall`] with its
/// inline result — the [`Content::ToolResult`] whose `output` is the
/// `{type:'call', …}` body lowered by [`parse_mcp_call`] — back into a single
/// Responses `mcp_call` output item, inverting the parse split. Returns `None`
/// when `result_output` is not such an MCP-call body (so a dynamic call routed in
/// from a different wire, with no matching MCP result, is not mis-recombined).
/// The emitted item restores `id`, `server_label`, `name`, `arguments`, and the
/// inline `output`/`error`.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/tool/mcp.ts>
fn render_mcp_call_item(
    call_id: &str,
    result_meta: &ProviderMetadata,
    result_output: &ToolResultOutput,
) -> Option<serde_json::Value> {
    let body = match result_output {
        ToolResultOutput::Json { value } => value.as_object()?,
        _ => return None,
    };
    if body.get("type").and_then(|t| t.as_str()) != Some(RESPONSES_MCP_CALL_TAG) {
        return None;
    }
    let mut item = serde_json::Map::new();
    item.insert("type".into(), "mcp_call".into());
    // Prefer the preserved item id; fall back to the correlating call id.
    let item_id = responses_item_id(result_meta).unwrap_or_else(|| call_id.to_string());
    item.insert("id".into(), item_id.into());
    if let Some(label) = body.get("serverLabel") {
        item.insert("server_label".into(), label.clone());
    }
    if let Some(name) = body.get("name") {
        item.insert("name".into(), name.clone());
    }
    if let Some(arguments) = body.get("arguments") {
        item.insert("arguments".into(), arguments.clone());
    }
    if let Some(output) = body.get("output") {
        item.insert("output".into(), output.clone());
    }
    if let Some(error) = body.get("error") {
        item.insert("error".into(), error.clone());
    }
    Some(serde_json::Value::Object(item))
}

/// Reconstruct a Responses `local_shell_call` output item from a client
/// `local_shell` [`Content::ToolCall`], inverting the `local_shell_call` parse.
/// The `action` is carried verbatim in the call's `{action}` input (so a
/// same-protocol round-trip is byte-faithful), `call_id` is the call id, and the
/// item `id` is restored from `provider_metadata["openai"]["itemId"]`. Returns
/// `None` when the input is not the expected `{action}` object.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
fn render_local_shell_call_item(c: &Content) -> Option<serde_json::Value> {
    let Content::ToolCall {
        id,
        arguments,
        provider_metadata,
        ..
    } = c
    else {
        return None;
    };
    let input: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let action = input.get("action")?.clone();
    let mut item = serde_json::Map::new();
    item.insert("type".into(), "local_shell_call".into());
    item.insert("call_id".into(), id.clone().into());
    // The item `id` is distinct from `call_id` on this wire; restore it when it
    // round-tripped, else fall back to the call id so the item is still valid.
    let item_id = responses_item_id(provider_metadata).unwrap_or_else(|| id.clone());
    item.insert("id".into(), item_id.into());
    item.insert("action".into(), action);
    Some(serde_json::Value::Object(item))
}

/// The Responses protocol adapter.
pub struct ResponsesAdapter;

/// HTTP transport for Responses: `POST {api_base}/responses` with
/// `Authorization: Bearer <api_key>`.
pub struct ResponsesTransport;

// ===== wire request types =====

/// Responses request body
/// (<https://platform.openai.com/docs/api-reference/responses/create>).
///
/// `pub` so downstream crates (notably `bitrouter-cloud`) can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
///
/// `input` stays `serde_json::Value` on purpose — the Responses API accepts
/// either a plain string or a heterogeneous item array (Codex multi-turn,
/// #454-3), and the deeper walk happens in `parse_input` (private). Mirrors
/// how Anthropic's `system` / `content` are typed.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResponsesRequest {
    #[serde(default)]
    model: String,
    input: serde_json::Value,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    tools: Vec<ResponsesTool>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    reasoning: Option<ResponsesReasoningConfig>,
    /// Output `text` config — `format` is the structured-output constraint
    /// (`json_schema` is promoted to the canonical slot; `text` / `json_object`
    /// pass through). Sibling keys (e.g. `verbosity`) survive via `extra`.
    #[serde(default)]
    text: Option<ResponsesText>,
    #[serde(default)]
    stream: bool,
    /// Whether OpenAI may retain the request and response.
    #[serde(default)]
    store: Option<bool>,
    /// Whether the model may issue tool calls in parallel.
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    /// Every other field — `tool_choice`,
    /// `max_tool_calls`, `metadata`, `include[]`, `previous_response_id`,
    /// `stream_options`, … — rides through here and is splatted
    /// back on render. Skipped from the published schema so the documented
    /// contract is the set of typed fields; pass-through behavior is
    /// preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// Responses `text` config. `format` constrains output shape; sibling keys
/// (e.g. `verbosity`) pass through via `extra`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResponsesText {
    #[serde(default)]
    format: Option<ResponsesTextFormat>,
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// Responses `text.format`
/// (<https://platform.openai.com/docs/api-reference/responses/create>) — a
/// closed union over OpenAI's three output modes.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    /// Free-form text — the default.
    Text,
    /// Legacy JSON mode.
    JsonObject,
    /// JSON constrained to a schema.
    JsonSchema {
        /// Schema name (OpenAI requires it).
        name: String,
        /// Optional schema description — extra LLM guidance OpenAI passes to the
        /// model. Promoted to the canonical `response_format` description.
        /// <https://platform.openai.com/docs/api-reference/responses/create>
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Strict-mode flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// One element of [`ResponsesRequest`]'s `tools` array.
///
/// Responses uses one flat array for both kinds of tool:
/// - a **function** tool — `{type:"function", name, description?, parameters, strict?}`;
/// - a **provider-defined** server tool — `{type:"web_search_preview"|…, …config}`
///   (`code_interpreter`, `file_search`, `image_generation`, `computer_use_preview`).
///
/// `kind` (`type`) discriminates the two; for a server tool the configuration
/// keys ride in `extra` so they are preserved verbatim into the canonical
/// `Tool::ProviderDefined.args`.
/// <https://platform.openai.com/docs/api-reference/responses/create#responses-create-tools>
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ResponsesTool {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
    /// OpenAI strict-mode flag (V3 `strict`) on a function tool. Captured so it
    /// is not lost across the canonical boundary.
    #[serde(default)]
    strict: Option<bool>,
    /// Server-tool configuration keys (e.g. `search_context_size`,
    /// `container`, `vector_store_ids`) for a provider-defined tool. Preserved
    /// verbatim. Skipped from the published schema — the documented contract is
    /// the typed function-tool shape.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// `reasoning` knob on [`ResponsesRequest`] — only `effort` is read; other
/// fields (`summary`, …) round-trip via no slot today (matching v0 behavior).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ResponsesReasoningConfig {
    #[serde(default)]
    effort: Option<String>,
}

fn parse_role(role: &str) -> Result<Role> {
    match role {
        "system" | "developer" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(BitrouterError::bad_request(format!(
            "unknown responses message role '{other}'"
        ))),
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Parse a Responses `content` value (string or array of content parts) into
/// ordered canonical content.
fn parse_responses_content(value: Option<&serde_json::Value>) -> Vec<Content> {
    match value {
        Some(serde_json::Value::String(s)) => vec![Content::Text {
            text: s.clone(),
            provider_metadata: ProviderMetadata::new(),
        }],
        Some(serde_json::Value::Array(parts)) => {
            parts.iter().filter_map(parse_responses_part).collect()
        }
        _ => Vec::new(),
    }
}

/// Parse one Responses content part into canonical content.
/// <https://platform.openai.com/docs/api-reference/responses/create>
fn parse_responses_part(part: &serde_json::Value) -> Option<Content> {
    match part.get("type").and_then(|t| t.as_str())? {
        "input_text" | "output_text" | "text" => Some(Content::Text {
            text: part.get("text").and_then(|t| t.as_str())?.to_string(),
            provider_metadata: ProviderMetadata::new(),
        }),
        "input_image" => {
            let url = part.get("image_url").and_then(|u| u.as_str())?;
            let (media_type, data) = DataContent::from_url(url);
            // The Responses `input_image` carries the same `detail` hint
            // (`auto` | `low` | `high`) as Chat Completions' `image_url`; it is
            // provider metadata, not a payload field. Preserve it under the
            // `openai` namespace so it round-trips on a Responses request and
            // survives a hop from a Chat Completions client (same namespace).
            // <https://platform.openai.com/docs/api-reference/responses/create>
            let mut provider_metadata = ProviderMetadata::new();
            if let Some(detail) = part.get("detail").filter(|v| !v.is_null()) {
                set_provider_metadata(
                    &mut provider_metadata,
                    PROVIDER_ID_OPENAI,
                    "detail",
                    detail.clone(),
                );
            }
            Some(Content::File {
                media_type: media_type.unwrap_or_else(|| "image/*".to_string()),
                data,
                filename: None,
                provider_metadata,
            })
        }
        "input_file" => {
            let file_data = part.get("file_data").and_then(|d| d.as_str())?;
            let (media_type, data) = DataContent::from_url(file_data);
            Some(Content::File {
                media_type: media_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                data,
                filename: part
                    .get("filename")
                    .and_then(|f| f.as_str())
                    .map(str::to_string),
                provider_metadata: ProviderMetadata::new(),
            })
        }
        _ => None,
    }
}

/// Parse a Responses `output_text` part's `annotations[]` into canonical
/// [`Content::Source`] parts, with `next_index` tracking the running citation
/// count so synthesized ids stay unique across parts/items. Mirrors the AI SDK
/// OpenAI Responses mapping:
/// - `url_citation` (`{url, title}`) → [`Source::Url`];
/// - `file_citation` / `container_file_citation` (`{filename, file_id}`) →
///   [`Source::Document`] with `media_type: text/plain`, `title`/`filename` from
///   `filename`;
/// - `file_path` (`{file_id}`) → [`Source::Document`] with
///   `media_type: application/octet-stream`, `title`/`filename` from `file_id`.
///
/// The wire carries no citation id, so one is synthesized (from the url, or from
/// `file_id`/`filename` for documents) + index. The `index`/`start_index` text
/// offsets and the `file_id`/`container_id` provider fields have no canonical
/// slot and are dropped — see [`Source`].
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
fn parse_responses_annotations(
    annotations: Option<&serde_json::Value>,
    next_index: &mut usize,
) -> Vec<Content> {
    let Some(arr) = annotations.and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ann in arr {
        let source = match ann.get("type").and_then(|t| t.as_str()) {
            Some("url_citation") => {
                let Some(url) = ann.get("url").and_then(|u| u.as_str()) else {
                    continue;
                };
                Source::Url {
                    id: Source::synthesize_id(url, *next_index),
                    url: url.to_string(),
                    title: ann
                        .get("title")
                        .and_then(|t| t.as_str())
                        .map(str::to_string),
                }
            }
            Some("file_citation") | Some("container_file_citation") => {
                let filename = ann
                    .get("filename")
                    .and_then(|f| f.as_str())
                    .unwrap_or_default()
                    .to_string();
                Source::Document {
                    id: Source::synthesize_id(&filename, *next_index),
                    media_type: "text/plain".to_string(),
                    title: filename.clone(),
                    filename: (!filename.is_empty()).then_some(filename),
                }
            }
            Some("file_path") => {
                let file_id = ann
                    .get("file_id")
                    .and_then(|f| f.as_str())
                    .unwrap_or_default()
                    .to_string();
                Source::Document {
                    id: Source::synthesize_id(&file_id, *next_index),
                    media_type: "application/octet-stream".to_string(),
                    title: file_id.clone(),
                    filename: (!file_id.is_empty()).then_some(file_id),
                }
            }
            _ => continue,
        };
        out.push(Content::Source {
            source,
            provider_metadata: ProviderMetadata::new(),
        });
        *next_index += 1;
    }
    out
}

/// Render canonical [`Content::Source`] parts into a Responses `annotations[]`
/// array for an `output_text` part — the location [`parse_responses_annotations`]
/// reads. [`Source::Url`] → `url_citation`; [`Source::Document`] →
/// `file_citation` (keyed by `filename`). The `file_id`/`container_id` provider
/// fields cannot be reconstructed from the canonical `Source` and are omitted on
/// the document path (documented loss). Returns an empty Vec when the result
/// carries no sources.
/// <https://platform.openai.com/docs/api-reference/responses/object> (`annotations`)
fn render_responses_annotations(result: &GenerateResult) -> Vec<serde_json::Value> {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Source { source, .. } => Some(render_source_annotation(source)),
            _ => None,
        })
        .collect()
}

/// Render one canonical [`Source`] into a Responses `annotations[]` entry —
/// shared by the non-streaming [`render_responses_annotations`] and the
/// streaming `response.output_text.annotation.added` encode path.
/// [`Source::Url`] → `url_citation`; [`Source::Document`] → `file_citation`
/// (keyed by `filename`). The `file_id`/`container_id` provider fields cannot be
/// reconstructed from the canonical `Source` and are omitted on the document
/// path (documented loss).
/// <https://platform.openai.com/docs/api-reference/responses/object> (`annotations`)
fn render_source_annotation(source: &Source) -> serde_json::Value {
    match source {
        Source::Url { url, title, .. } => {
            let mut ann = serde_json::Map::new();
            ann.insert("type".into(), "url_citation".into());
            ann.insert("url".into(), url.clone().into());
            if let Some(title) = title {
                ann.insert("title".into(), title.clone().into());
            }
            serde_json::Value::Object(ann)
        }
        Source::Document {
            title, filename, ..
        } => {
            let name = filename.clone().unwrap_or_else(|| title.clone());
            serde_json::json!({
                "type": "file_citation",
                "filename": name,
            })
        }
    }
}

/// Parse the Responses `input` field — a string or a heterogeneous item array.
/// Lenient by design (#454-3): item shapes that are not recognised are skipped,
/// not rejected.
fn parse_input(value: &serde_json::Value) -> Result<Vec<Message>> {
    match value {
        serde_json::Value::String(s) => Ok(vec![Message::text(Role::User, s.clone())]),
        serde_json::Value::Array(items) => {
            let mut messages = Vec::new();
            for item in items {
                let item_type = item.get("type").and_then(|t| t.as_str());
                match item_type {
                    // a plain message item, or an item with no `type` but a `role`
                    Some("message") | None => {
                        if let Some(role) = item.get("role").and_then(|r| r.as_str()) {
                            let role = parse_role(role)?;
                            let content = parse_responses_content(item.get("content"));
                            messages.push(Message { role, content });
                        }
                    }
                    Some("function_call") => {
                        messages.push(Message {
                            role: Role::Assistant,
                            // tool calls are single-part assistant turns
                            content: vec![Content::ToolCall {
                                id: item
                                    .get("call_id")
                                    .or_else(|| item.get("id"))
                                    .and_then(|i| i.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                name: item
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                arguments: item
                                    .get("arguments")
                                    .and_then(|a| a.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                // A `function_call` input item is always a client
                                // tool call; provider-executed server-tool calls
                                // (`web_search_call`, …) are never re-sent as
                                // input items.
                                provider_executed: false,
                                // …nor is it a provider-executed MCP (`dynamic`)
                                // call (those arrive as `mcp_call` items).
                                dynamic: false,
                                provider_metadata: ProviderMetadata::new(),
                            }],
                        });
                    }
                    // OpenAI Responses `function_call_output {call_id, output}`:
                    // `output` is a string, a content-part array, or (loosely) a
                    // bare JSON value, with no tool name and no error flag on the
                    // wire. A string → Text, a part array → Content, any other
                    // value → Json.
                    // <https://platform.openai.com/docs/api-reference/responses/create>
                    Some("function_call_output") => {
                        let output = item
                            .get("output")
                            .map(parse_responses_tool_output)
                            .unwrap_or_else(|| ToolResultOutput::Text {
                                value: String::new(),
                            });
                        messages.push(Message {
                            role: Role::Tool,
                            content: vec![Content::ToolResult {
                                call_id: item
                                    .get("call_id")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                tool_name: None,
                                output,
                                // A `function_call_output` is a plain client
                                // tool-result item, never an inline MCP result.
                                dynamic: false,
                                provider_metadata: ProviderMetadata::new(),
                            }],
                        });
                    }
                    // OpenAI Responses `mcp_approval_response {approval_request_id,
                    // approve}` — the human-in-the-loop grant/deny for a
                    // provider-executed MCP tool call. It becomes a
                    // `Content::ToolApprovalResponse` (a `tool`-role part). A
                    // **denial** (`approve == false`) additionally yields a paired
                    // `ToolResult` whose output is `ExecutionDenied`, carrying the
                    // approval id under `provider_metadata["openai"]["approvalId"]`
                    // so render knows the denial was already conveyed by the
                    // approval item and skips re-emitting it as a string — matching
                    // the AI SDK's `execution-denied` skip rule.
                    // <https://platform.openai.com/docs/api-reference/responses/object>
                    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
                    Some("mcp_approval_response") => {
                        let approval_id = item
                            .get("approval_request_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let approved = item
                            .get("approve")
                            .and_then(|a| a.as_bool())
                            .unwrap_or(false);
                        let mut content = vec![Content::ToolApprovalResponse {
                            approval_id: approval_id.clone(),
                            approved,
                            reason: None,
                            provider_metadata: ProviderMetadata::new(),
                        }];
                        if !approved {
                            let mut denial_meta = ProviderMetadata::new();
                            set_provider_metadata(
                                &mut denial_meta,
                                PROVIDER_ID_OPENAI,
                                "approvalId",
                                approval_id.clone().into(),
                            );
                            content.push(Content::ToolResult {
                                call_id: approval_id,
                                tool_name: None,
                                output: ToolResultOutput::ExecutionDenied { reason: None },
                                // An approval denial is not an MCP inline result.
                                dynamic: false,
                                provider_metadata: denial_meta,
                            });
                        }
                        messages.push(Message {
                            role: Role::Tool,
                            content,
                        });
                    }
                    Some("reasoning") => {
                        let text = item
                            .get("summary")
                            .and_then(|s| s.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        if !text.is_empty() {
                            messages.push(Message {
                                role: Role::Assistant,
                                content: vec![Content::Reasoning {
                                    text,
                                    provider_metadata: ProviderMetadata::new(),
                                }],
                            });
                        }
                    }
                    // An `mcp_call` echoed back into the request `input[]` (a
                    // stateless client replaying the assistant turn) is lowered to
                    // the same `dynamic` `ToolCall` + inline `ToolResult` pair as
                    // on the response side, carried as one assistant message. This
                    // is symmetric in IR-lowering only: `render_request` then drops
                    // the dynamic pair (a provider-executed call is not replayed on
                    // the input wire, mirroring the AI SDK), so the pair survives a
                    // request→response re-render, not a request→request one.
                    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
                    Some("mcp_call") => {
                        messages.push(Message {
                            role: Role::Assistant,
                            content: parse_mcp_call(item),
                        });
                    }
                    // A `local_shell_call` input item — a client shell call replayed
                    // into the request. Mapped to the same client `local_shell`
                    // `ToolCall` as the response parse, as an assistant message.
                    Some("local_shell_call") => {
                        let call_id = item
                            .get("call_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let action = item.get("action").cloned().unwrap_or(serde_json::json!({}));
                        let mut meta = ProviderMetadata::new();
                        if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                            set_provider_metadata(
                                &mut meta,
                                PROVIDER_ID_OPENAI,
                                RESPONSES_ITEM_ID,
                                serde_json::Value::String(id.to_string()),
                            );
                        }
                        messages.push(Message {
                            role: Role::Assistant,
                            content: vec![Content::ToolCall {
                                id: call_id,
                                name: RESPONSES_LOCAL_SHELL_TOOL.to_string(),
                                arguments: serde_json::json!({ "action": action }).to_string(),
                                provider_executed: false,
                                dynamic: false,
                                provider_metadata: meta,
                            }],
                        });
                    }
                    // forward-compatible: unknown item types are skipped, not fatal
                    Some(_) => {}
                }
            }
            Ok(messages)
        }
        _ => Err(BitrouterError::bad_request(
            "responses 'input' must be a string or an array of items",
        )),
    }
}

/// Map OpenAI's `error.type` (from the Responses error envelope) to a HTTP
/// status code so 4xx upstream errors are not silently 502'd. Spec:
/// <https://platform.openai.com/docs/guides/error-codes> +
/// <https://platform.openai.com/docs/api-reference/responses-streaming/response/failed>.
fn openai_error_status(err_type: &str) -> u16 {
    match err_type {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "rate_limit_error" | "tokens_limit_error" | "requests_limit_error" => 429,
        "server_error" | "api_error" | "" => 502,
        _ => 502,
    }
}

fn openai_policy_violation(error: Option<&serde_json::Value>, message: &str) -> bool {
    let policy_code = error
        .and_then(|value| value.get("code").or_else(|| value.get("type")))
        .and_then(|value| value.as_str())
        .is_some_and(|value| {
            matches!(
                value,
                "content_policy_violation" | "content_filter" | "safety_violation"
            )
        });
    let normalized = message.to_ascii_lowercase();
    policy_code
        || normalized.contains("flagged for possible cybersecurity risk")
        || normalized.contains("content policy violation")
}

// Responses has no native finish-reason *string* to preserve: its terminal
// signal is the response `status` (`completed` / `incomplete` / `failed`), a
// small closed set that the unified [`FinishReason`] enum reproduces exactly on
// render (`Length` → `incomplete`, `Error` → `failed`, else `completed`). There
// is therefore no lossy mapping to stash a raw value for — unlike the
// Messages / Chat Completions / Generate Content adapters, which collapse
// several distinct native reasons onto one variant and so stash
// `rawFinishReason`. Hence this adapter writes no raw finish reason.
fn finish_from_status(status: &str) -> Option<FinishReason> {
    match status {
        "completed" => Some(FinishReason::Stop),
        // #432: `incomplete` is a valid terminal status, not an error.
        "incomplete" => Some(FinishReason::Length),
        _ => None,
    }
}

impl InboundAdapter for ResponsesAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Responses
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        // The envelope deserializes strictly; `input` stays `Value` and is
        // walked leniently by `parse_input` so unexpected Codex-style item
        // shapes never cause a hard 400 (#454-3).
        let req: ResponsesRequest = serde_json::from_value(body.clone())
            .map_err(|e| describe_deser_error("ResponsesRequest", &e, &body))?;

        let messages = parse_input(&req.input)?;
        let system = req.instructions.filter(|s| !s.is_empty());

        // Responses packs function tools and provider-defined ("server") tools
        // into one flat array, discriminated by `type`. `type:"function"` (or a
        // typeless entry that still carries a `name`, tolerated for lenient
        // clients) becomes a `Tool::Function`; every other `type`
        // (`web_search_preview`, `code_interpreter`, `file_search`,
        // `image_generation`, `computer_use_preview`, …) is a provider-defined
        // tool whose config keys ride in `extra` and are preserved verbatim under
        // an `openai.<type>` id.
        // <https://platform.openai.com/docs/api-reference/responses/create#responses-create-tools>
        let tools = req
            .tools
            .into_iter()
            .filter_map(parse_responses_tool)
            .collect();

        // `text` is a typed field. Promote `text.format: json_schema` into the
        // canonical slot; other formats and sibling keys (e.g. `verbosity`)
        // re-attach to `extra["text"]` to pass through on render.
        let mut extra = req.extra;
        let response_format = match req.text {
            Some(ResponsesText {
                format,
                extra: text_extra,
            }) => match format {
                Some(ResponsesTextFormat::JsonSchema {
                    name,
                    description,
                    strict,
                    schema,
                }) => {
                    if !text_extra.is_empty() {
                        extra.insert(
                            "text".to_string(),
                            serde_json::Value::Object(text_extra.into_iter().collect()),
                        );
                    }
                    Some(ResponseFormat::JsonSchema {
                        name: Some(name),
                        description,
                        strict,
                        schema,
                    })
                }
                other => {
                    let mut text_map: serde_json::Map<String, serde_json::Value> =
                        text_extra.into_iter().collect();
                    if let Some(f) = other
                        && let Ok(v) = serde_json::to_value(&f)
                    {
                        text_map.insert("format".to_string(), v);
                    }
                    if !text_map.is_empty() {
                        extra.insert("text".to_string(), serde_json::Value::Object(text_map));
                    }
                    None
                }
            },
            None => None,
        };

        // Promote a known-shape `tool_choice` into the canonical slot so it can
        // translate across protocols; unmapped shapes (hosted-tool selectors,
        // `allowed_tools`, …) stay in `extra` and pass through.
        let tool_choice = parse_responses_tool_choice(&mut extra);

        Ok(Prompt {
            model: req.model,
            system,
            // Responses carries no system-level `cache_control` on its wire.
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_output_tokens,
                chat_token_limit_field: None,
                reasoning_effort: req.reasoning.and_then(|r| r.effort),
                response_modalities: Vec::new(),
                // The Responses API has no top-level top_k / seed / stop /
                // presence_penalty / frequency_penalty — they are unsupported on
                // this wire and dropped by the reference implementation, so the
                // canonical slots stay empty here and the outbound adapter
                // renders none of them.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                top_k: None,
                seed: None,
                stop: Vec::new(),
                presence_penalty: None,
                frequency_penalty: None,
                store: req.store,
                parallel_tool_calls: req.parallel_tool_calls,
                chat_stream_options: None,
                extra_protocol: Some(ApiProtocol::Responses),
                supplemental_extra: HashMap::new(),
                // Splat every remaining Responses-API field without a typed slot
                // — max_tool_calls, metadata, include[], previous_response_id,
                // stream_options, … — into `extra`
                // so render_request can put them back.
                extra,
            },
            response_format,
            tool_choice,
            stream: req.stream,
        })
    }

    fn render_response(
        &self,
        result: &GenerateResult,
        prompt: &Prompt,
        request_id: &str,
    ) -> Result<serde_json::Value> {
        let output = render_output_items(result);
        let response_id = encode_gateway_continuation_id(request_id)?;
        let mut body = serde_json::json!({
            "id": response_id,
            "object": "response",
            "created_at": created_at_unix_seconds(),
            "model": prompt.model,
            "status": match &result.finish_reason {
                Some(FinishReason::Length) => "incomplete",
                Some(FinishReason::Error(_)) => "failed",
                _ => "completed",
            },
            "output": output,
        });
        // Mirror the streaming `emit_terminal` behaviour: omit the `usage`
        // key entirely when the upstream reported no token counts. A
        // zero-filled object would let downstream callers conclude the
        // request used zero tokens.
        if let Some(usage) = &result.usage {
            body["usage"] = render_responses_usage(usage);
        }
        Ok(body)
    }

    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder> {
        Box::new(ResponsesStreamEncoder {
            response_id: encode_gateway_continuation_id(request_id),
            model: model.to_string(),
            seq: 0,
            created: false,
            next_output_index: 0,
            reasoning_item: None,
            text_item: None,
            tool_item: None,
            completed_items: Vec::new(),
            annotation_index: 0,
            pending_mcp_call: None,
        })
    }
}

impl OutboundAdapter for ResponsesAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Responses
    }

    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value> {
        let mut input = Vec::new();
        for m in &prompt.messages {
            input.extend(render_message_items(m));
        }
        let mut req = serde_json::Map::new();
        req.insert("model".into(), prompt.model.clone().into());
        req.insert("input".into(), input.into());
        if let Some(system) = &prompt.system {
            req.insert("instructions".into(), system.clone().into());
        }
        if !prompt.tools.is_empty() {
            req.insert(
                "tools".into(),
                prompt
                    .tools
                    .iter()
                    .map(render_responses_tool)
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        if let Some(t) = prompt.params.temperature {
            req.insert("temperature".into(), t.into());
        }
        if let Some(p) = prompt.params.top_p {
            req.insert("top_p".into(), p.into());
        }
        if let Some(mt) = prompt.params.max_tokens {
            req.insert("max_output_tokens".into(), mt.into());
        }
        if let Some(re) = &prompt.params.reasoning_effort {
            req.insert("reasoning".into(), serde_json::json!({ "effort": re }));
        }
        if let Some(store) = prompt.params.store {
            req.insert("store".into(), store.into());
        }
        if let Some(parallel) = prompt.params.parallel_tool_calls {
            req.insert("parallel_tool_calls".into(), parallel.into());
        }
        // Render the canonical response_format into Responses' `text.format`.
        // Merge with any extras-supplied `text` blob so caller-supplied
        // sibling keys (e.g. `verbosity`) survive.
        if let Some(rf) = &prompt.response_format {
            let mut text = prompt
                .params
                .extra_value_for_protocol(&ApiProtocol::Responses, "text")
                .and_then(|t| t.as_object().cloned())
                .unwrap_or_default();
            text.insert("format".into(), render_responses_response_format(rf));
            req.insert("text".into(), serde_json::Value::Object(text));
        }
        // Render the canonical tool_choice into Responses' native shape, before
        // the extras splat so it wins over any leftover `tool_choice`.
        if let Some(tc) = &prompt.tool_choice {
            req.insert("tool_choice".into(), render_responses_tool_choice(tc));
        }
        // Splat Responses-API extras (metadata, include, …) only for a native
        // round trip. Typed fields win.
        for (k, v) in prompt.params.extras_for_protocol(&ApiProtocol::Responses) {
            req.entry(k.clone()).or_insert_with(|| v.clone());
        }
        req.insert("stream".into(), prompt.stream.into());
        Ok(serde_json::Value::Object(req))
    }

    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult> {
        let output = body
            .get("output")
            .and_then(|o| o.as_array())
            .ok_or_else(|| BitrouterError::bad_request("responses response missing 'output'"))?;
        let mut content = Vec::new();
        // Running citation counter so synthesized source ids stay unique across
        // every annotated output_text part in the response.
        let mut source_index = 0usize;
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                content.push(Content::Text {
                                    text: text.to_string(),
                                    provider_metadata: ProviderMetadata::new(),
                                });
                            }
                            // An `output_text` part may carry web-search / file
                            // citations on `annotations[]`; lift them into
                            // `Content::Source` parts right after the text.
                            content.extend(parse_responses_annotations(
                                part.get("annotations"),
                                &mut source_index,
                            ));
                        }
                    }
                }
                Some("reasoning") => {
                    let text = item
                        .get("summary")
                        .and_then(|s| s.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        content.push(Content::Reasoning {
                            text,
                            provider_metadata: ProviderMetadata::new(),
                        });
                    }
                }
                Some("function_call") => {
                    content.push(Content::ToolCall {
                        id: item
                            .get("call_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: item
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        // A `function_call` output item is a client tool call.
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
                // OpenAI Responses built-in (server-side) tools surface as their
                // own output-item types rather than `function_call`. The SDK
                // exposes each as a provider-executed tool call keyed by item
                // `id`, with a synthetic tool name and the call's distinguishing
                // input, matching the AI SDK reference mapping. These must be
                // marked `provider_executed` so they are not re-sent as client
                // `function_call` items on a later turn. `image_generation_call`
                // and `computer_call` join the no-echoed-input group: the AI SDK
                // emits an empty input for both (`'{}'` and `''` respectively)
                // and surfaces their payload only via a *separate* tool-result
                // (the generated image / the computer-use status). The flat
                // `ToolCall` cannot carry that result, so — exactly as for
                // `web_search_call`/`file_search_call` — only the call itself is
                // modeled here; it round-trips via `render_output_items`, which
                // re-emits `<name>_call` keyed by `id`.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                Some(
                    server_tool @ ("web_search_call"
                    | "file_search_call"
                    | "image_generation_call"
                    | "computer_call"),
                ) => {
                    content.push(Content::ToolCall {
                        id: item
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: server_tool.trim_end_matches("_call").to_string(),
                        // The Responses API does not echo these tools' query
                        // arguments on the output item, so the SDK reference
                        // emits an empty input object.
                        arguments: "{}".to_string(),
                        provider_executed: true,
                        // OpenAI's built-in server tools are not runtime MCP tools.
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
                Some("code_interpreter_call") => {
                    // `code_interpreter_call` does carry its `code` and
                    // `container_id`; preserve them as the call input (matching
                    // the AI SDK `{ code, containerId }` shape).
                    let mut input = serde_json::Map::new();
                    if let Some(code) = item.get("code").and_then(|c| c.as_str()) {
                        input.insert("code".into(), code.into());
                    }
                    if let Some(container) = item.get("container_id").and_then(|c| c.as_str()) {
                        input.insert("containerId".into(), container.into());
                    }
                    content.push(Content::ToolCall {
                        id: item
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: "code_interpreter".to_string(),
                        arguments: serde_json::Value::Object(input).to_string(),
                        provider_executed: true,
                        // `code_interpreter` is a built-in server tool, not MCP.
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
                // OpenAI Responses `mcp_approval_request {id, server_label, name,
                // arguments, approval_request_id?}` — a provider-executed MCP tool
                // call paused for human approval. It becomes a
                // `Content::ToolApprovalRequest`. The MCP server identity
                // (`server_label` / `name` / `arguments`) has no slot on the flat
                // content shape, so it rides in `provider_metadata["openai"]` to
                // reproduce the exact item on render.
                //
                // The wire carries TWO ids that can differ: the item `id` and the
                // optional `approval_request_id` (the correlation key the later
                // `mcp_approval_response.approval_request_id` and any `mcp_call`
                // reference). `approval_id` takes the correlation key —
                // `approval_request_id`, falling back to `id` — matching the AI SDK
                // reference (`approval_request_id ?? id`). When the item ALSO had a
                // distinct `id`, that raw item id would otherwise be lost on a
                // same-protocol round-trip (render keys the item by `approval_id`),
                // so it is preserved under `provider_metadata["openai"]["itemId"]`
                // — the same key the AI SDK uses for the OpenAI item id — and
                // restored on render. `tool_call_id` is synthesized from
                // `approval_id` (the wire carries no separate tool-call id),
                // mirroring the AI SDK reference which generates a fresh id here.
                // <https://platform.openai.com/docs/api-reference/responses/object>
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                Some("mcp_approval_request") => {
                    let item_id = item.get("id").and_then(|i| i.as_str());
                    let approval_id = item
                        .get("approval_request_id")
                        .and_then(|i| i.as_str())
                        .or(item_id)
                        .unwrap_or_default()
                        .to_string();
                    let mut meta = ProviderMetadata::new();
                    for key in ["server_label", "name", "arguments"] {
                        if let Some(v) = item.get(key) {
                            // Store under the camelCase form the AI SDK uses for
                            // the OpenAI namespace (`serverLabel`).
                            let meta_key = if key == "server_label" {
                                "serverLabel"
                            } else {
                                key
                            };
                            set_provider_metadata(
                                &mut meta,
                                PROVIDER_ID_OPENAI,
                                meta_key,
                                v.clone(),
                            );
                        }
                    }
                    // Preserve the raw item `id` only when it differs from the
                    // chosen `approval_id`, so it round-trips without bloating the
                    // common case (where the two coincide and `id` is recoverable).
                    if let Some(id) = item_id
                        && id != approval_id
                    {
                        set_provider_metadata(
                            &mut meta,
                            PROVIDER_ID_OPENAI,
                            "itemId",
                            serde_json::Value::String(id.to_string()),
                        );
                    }
                    content.push(Content::ToolApprovalRequest {
                        tool_call_id: synthesize_approval_tool_call_id(&approval_id),
                        approval_id,
                        provider_metadata: meta,
                    });
                }
                // `mcp_call` — a provider-executed remote MCP tool call whose
                // *result* is carried **inline** on the same item
                // (`{ server_label, name, arguments, output?, error? }`). It is
                // lowered (mirroring the AI SDK reference) to a `dynamic`,
                // provider-executed `ToolCall` PLUS a paired `ToolResult` whose
                // body is the MCP-specific `{ type: 'call', serverLabel, name,
                // arguments, output?, error? }` structure. `render_output_items`
                // recombines the two back into a single `mcp_call` item, so the
                // inline result round-trips same-protocol exactly.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                Some("mcp_call") => content.extend(parse_mcp_call(item)),
                // `mcp_list_tools` — the catalogue of tools a remote MCP server
                // advertised. The AI SDK reference skips it (it is neither a call
                // nor a result the model acts on), and so does bitrouter; there is
                // no canonical content part for a tool catalogue.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                Some("mcp_list_tools") => {}
                // `local_shell_call` — a *client*-executed shell tool call (the AI
                // SDK leaves `providerExecuted` unset and keys it by `call_id`),
                // carrying an `action` payload. It maps to an ordinary client
                // `ToolCall` named `local_shell` whose input is `{action}` (the
                // wire action preserved verbatim); `render_output_items`
                // reconstructs the `local_shell_call` item from the same name, so
                // the call round-trips same-protocol. The item `id` (distinct from
                // `call_id`) rides in `provider_metadata["openai"]["itemId"]`.
                //
                // Residual gap: the paired `local_shell_call_output` is a
                // *client*-supplied result on a follow-up **request**. bitrouter's
                // `ToolResult` does not carry the tool name on the Responses wire
                // (which keys results purely by `call_id`), so the request render
                // cannot distinguish a `local_shell` result from an ordinary
                // `function_call_output`; the output therefore degrades to
                // `function_call_output` rather than `local_shell_call_output`.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/tool/local-shell.ts>
                Some("local_shell_call") => {
                    let call_id = item
                        .get("call_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let action = item.get("action").cloned().unwrap_or(serde_json::json!({}));
                    let mut meta = ProviderMetadata::new();
                    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                        set_provider_metadata(
                            &mut meta,
                            PROVIDER_ID_OPENAI,
                            RESPONSES_ITEM_ID,
                            serde_json::Value::String(id.to_string()),
                        );
                    }
                    content.push(Content::ToolCall {
                        id: call_id,
                        name: RESPONSES_LOCAL_SHELL_TOOL.to_string(),
                        arguments: serde_json::json!({ "action": action }).to_string(),
                        // A `local_shell_call` is a client tool call (the client
                        // runs the shell), not a provider-executed or MCP call.
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: meta,
                    });
                }
                // Any other unknown item type is skipped for forward
                // compatibility (mirrors the input-side `Some(_) => {}`), rather
                // than failing the whole response.
                _ => {}
            }
        }
        let finish_reason = body
            .get("status")
            .and_then(|s| s.as_str())
            .and_then(finish_from_status);
        let usage = body.get("usage").and_then(parse_usage);
        // Responses: top-level `id` (`resp_...`).
        // <https://platform.openai.com/docs/api-reference/responses/object>
        let response_id = body
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details: None,
            // The Responses object carries no result-level field that lacks a
            // dedicated canonical slot (no `system_fingerprint`, unlike Chat
            // Completions), so result-level provider metadata is empty here.
            provider_metadata: ProviderMetadata::new(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

/// Parse one Responses `tools` entry into a canonical [`Tool`]. A
/// `type:"function"` entry (or a typeless one that still has a `name`) is a
/// [`Tool::Function`]; anything else is a provider-defined server tool keyed by
/// its `type`, namespaced `openai.<type>`, with its config keys preserved
/// verbatim as `args`. An entry that is neither (no `type`, no `name`) is
/// dropped — there is nothing to forward.
fn parse_responses_tool(t: ResponsesTool) -> Option<Tool> {
    let is_function = t.kind.as_deref() == Some("function");
    if is_function || (t.kind.is_none() && t.name.is_some()) {
        return Some(Tool::Function {
            name: t.name.unwrap_or_default(),
            description: t.description,
            parameters: if t.parameters.is_null() {
                serde_json::json!({})
            } else {
                t.parameters
            },
            strict: t.strict,
            provider_metadata: ProviderMetadata::new(),
        });
    }
    // Provider-defined server tool. `type` is the tool kind (and the tool name);
    // config keys live in `extra`. A few server tools (`file_search`) accept a
    // distinct `name`; default to the kind when absent.
    let kind = t.kind?;
    let mut args: serde_json::Map<String, serde_json::Value> = t.extra.into_iter().collect();
    if let Some(description) = t.description {
        args.insert("description".into(), description.into());
    }
    if !t.parameters.is_null() {
        args.insert("parameters".into(), t.parameters);
    }
    if let Some(strict) = t.strict {
        args.insert("strict".into(), strict.into());
    }
    Some(Tool::ProviderDefined {
        id: format!("{PROVIDER_ID_OPENAI}.{kind}"),
        name: t.name.unwrap_or_else(|| kind.clone()),
        args: serde_json::Value::Object(args),
        provider_metadata: ProviderMetadata::new(),
    })
}

/// Render one canonical [`Tool`] into a Responses `tools` entry.
///
/// A [`Tool::Function`] becomes the flat `{type:"function", name, description?,
/// parameters, strict?}` shape; `strict` is emitted when set (previously always
/// dropped). A [`Tool::ProviderDefined`] is rendered to its source-native shape
/// via [`provider_defined_native`]: for an `openai.*` id this is the exact
/// Responses server-tool object (`{type:<tool>, …args}`), a lossless
/// same-protocol round-trip; a foreign-provider id is preserved verbatim
/// (faithful passthrough) so the upstream decides.
/// <https://platform.openai.com/docs/api-reference/responses/create#responses-create-tools>
fn render_responses_tool(tool: &Tool) -> serde_json::Value {
    match tool {
        Tool::Function {
            name,
            description,
            parameters,
            strict,
            ..
        } => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), "function".into());
            obj.insert("name".into(), name.clone().into());
            obj.insert(
                "description".into(),
                description
                    .clone()
                    .map_or(serde_json::Value::Null, serde_json::Value::String),
            );
            obj.insert("parameters".into(), parameters.clone());
            if let Some(strict) = strict {
                obj.insert("strict".into(), (*strict).into());
            }
            serde_json::Value::Object(obj)
        }
        Tool::ProviderDefined { id, name, args, .. } => provider_defined_native(id, name, args),
    }
}

/// Render a canonical [`ResponseFormat`] into Responses' native
/// `{ type: "json_schema", name, description?, strict?, schema }` body that sits
/// under `text.format`. OpenAI requires `name`; default it. `description` is
/// emitted only when the canonical slot carries it.
/// <https://platform.openai.com/docs/api-reference/responses/create>
fn render_responses_response_format(rf: &ResponseFormat) -> serde_json::Value {
    let ResponseFormat::JsonSchema {
        name,
        description,
        strict,
        schema,
    } = rf;
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), "json_schema".into());
    obj.insert(
        "name".into(),
        name.clone()
            .unwrap_or_else(|| "response".to_string())
            .into(),
    );
    if let Some(description) = description {
        obj.insert("description".into(), description.clone().into());
    }
    if let Some(strict) = strict {
        obj.insert("strict".into(), (*strict).into());
    }
    obj.insert("schema".into(), schema.clone());
    serde_json::Value::Object(obj)
}

/// Promote a Responses `tool_choice` into the canonical [`ToolChoice`], removing
/// it from `extra` when it maps to a known shape. Hosted-tool / `allowed_tools`
/// selectors are left untouched so they pass through opaquely.
/// <https://platform.openai.com/docs/api-reference/responses/create#responses-create-tool_choice>
fn parse_responses_tool_choice(
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) -> Option<ToolChoice> {
    let parsed = match extra.get("tool_choice")? {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "required" => Some(ToolChoice::Required),
            "none" => Some(ToolChoice::None),
            _ => None,
        },
        serde_json::Value::Object(o)
            if o.get("type").and_then(|t| t.as_str()) == Some("function") =>
        {
            // Responses carries the forced function name flat on the object,
            // not nested under `function` as Chat Completions does.
            o.get("name")
                .and_then(|n| n.as_str())
                .map(|name| ToolChoice::Tool {
                    name: name.to_string(),
                })
        }
        _ => None,
    };
    if parsed.is_some() {
        extra.remove("tool_choice");
    }
    parsed
}

/// Render the canonical [`ToolChoice`] into Responses' native shape: the bare
/// strings `auto` / `required` / `none`, or `{ type: "function", name }` to
/// force one tool (flat `name`, unlike Chat Completions' nested form).
fn render_responses_tool_choice(tc: &ToolChoice) -> serde_json::Value {
    match tc {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Tool { name } => serde_json::json!({ "type": "function", "name": name }),
    }
}

#[async_trait]
impl Transport for ResponsesTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Responses
    }

    fn endpoint_url(&self, target: &RoutingTarget, _stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        format!("{base}/responses")
    }

    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let key = target.effective_api_key();
        let value = format!("Bearer {key}");
        let header = reqwest::header::HeaderValue::from_str(&value).map_err(|e| {
            BitrouterError::internal(format!("invalid api key for Authorization header: {e}"))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, header);
        Ok(request)
    }
}

/// Render one canonical message into zero or more Responses `input` items.
fn render_message_items(m: &Message) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    let mut text_parts = Vec::new();
    for c in &m.content {
        match c {
            Content::Text { text, .. } => {
                let kind = if m.role == Role::Assistant {
                    "output_text"
                } else {
                    "input_text"
                };
                text_parts.push(serde_json::json!({ "type": kind, "text": text }));
            }
            Content::Reasoning { .. } => {
                // reasoning is not re-sent as input; drop on the request side
            }
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_executed,
                ..
            } => {
                // A provider-executed server-tool call is NOT re-sent as a client
                // `function_call` input item — the provider already ran it, and
                // the Responses input wire has no slot to replay a server-tool
                // call (the AI SDK reference drops it / emits an item_reference).
                // This also covers a `dynamic` provider-executed MCP call.
                // Only client tool calls round-trip as input items.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
                if !provider_executed {
                    // A client `local_shell` call reproduces its `local_shell_call`
                    // input item (with the `action` payload), matching the AI SDK;
                    // every other client call is a `function_call`.
                    if name == RESPONSES_LOCAL_SHELL_TOOL
                        && let Some(item) = render_local_shell_call_item(c)
                    {
                        items.push(item);
                    } else {
                        items.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            // image/* -> `input_image`, other media -> `input_file`; the payload
            // is a URL or `data:` URI via the shared helper.
            // <https://platform.openai.com/docs/api-reference/responses/create>
            Content::File {
                media_type,
                data,
                filename,
                provider_metadata,
            } => {
                let part = if media_type.starts_with("image/") {
                    let mut image = serde_json::json!({
                        "type": "input_image", "image_url": data.to_url(media_type)
                    });
                    // Restore the OpenAI `detail` hint from the `openai`
                    // namespace when it round-tripped through `provider_metadata`
                    // (set by this protocol's parse path, or by Chat Completions'
                    // image_url parse — same namespace).
                    // <https://platform.openai.com/docs/api-reference/responses/create>
                    if let Some(detail) = provider_namespace(provider_metadata, PROVIDER_ID_OPENAI)
                        .and_then(|o| o.get("detail"))
                        && let Some(obj) = image.as_object_mut()
                    {
                        obj.insert("detail".into(), detail.clone());
                    }
                    image
                } else {
                    let mut file = serde_json::json!({
                        "type": "input_file", "file_data": data.to_url(media_type)
                    });
                    if let Some(name) = filename {
                        file["filename"] = serde_json::Value::String(name.clone());
                    }
                    file
                };
                text_parts.push(part);
            }
            // Responses `function_call_output {call_id, output}`. `output` is a
            // string or content-part array, with no error flag and no tool name,
            // so an error output degrades to its value and `tool_name` is dropped.
            // `Json` / `ErrorJson` stringify; `Content` becomes a part array.
            //
            // An `ExecutionDenied` output paired with an approval (its approval id
            // rides in `provider_metadata["openai"]["approvalId"]`) is **skipped**
            // here: the sibling `ToolApprovalResponse` already re-emits the
            // `mcp_approval_response` that conveys the denial, so emitting a
            // `function_call_output` too would duplicate it. An *unpaired* denial
            // (no approval id) falls through and degrades to the denial string.
            // <https://platform.openai.com/docs/api-reference/responses/create>
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
            Content::ToolResult {
                call_id,
                output,
                dynamic,
                provider_metadata,
                ..
            } => {
                // A `dynamic` MCP result is the inline result of a
                // provider-executed `mcp_call`; the provider already ran it, so it
                // is NOT re-sent as a client `function_call_output` (the AI SDK
                // reference likewise does not replay provider-executed results on
                // the input wire). It rides the response path instead, where the
                // `mcp_call` item recombines its call and inline result.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
                let denial_paired_with_approval =
                    matches!(output, ToolResultOutput::ExecutionDenied { .. })
                        && provider_namespace(provider_metadata, PROVIDER_ID_OPENAI)
                            .is_some_and(|o| o.contains_key("approvalId"));
                if !*dynamic && !denial_paired_with_approval {
                    let output_value = render_responses_tool_output(output);
                    items.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output_value,
                    }));
                }
            }
            // Responses `mcp_approval_response {approval_request_id, approve}` —
            // the grant/deny for a provider-executed MCP tool call, re-emitted on
            // the input wire. The optional canonical `reason` has no slot on this
            // item and is dropped (the wire carries none).
            // <https://platform.openai.com/docs/api-reference/responses/object>
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
            Content::ToolApprovalResponse {
                approval_id,
                approved,
                ..
            } => {
                items.push(serde_json::json!({
                    "type": "mcp_approval_response",
                    "approval_request_id": approval_id,
                    "approve": approved,
                }));
            }
            // A `mcp_approval_request` is an *output* item (an assistant reply), so
            // it never appears in a request `input` array — the request render
            // skips it. It round-trips via the response path
            // (`render_output_items`).
            Content::ToolApprovalRequest { .. } => {}
            // Citation sources are response-side metadata only; they are never
            // re-sent as a request input item, so the request path skips them.
            Content::Source { .. } => {}
        }
    }
    if !text_parts.is_empty() {
        items.insert(
            0,
            serde_json::json!({
                "type": "message",
                "role": role_str(m.role),
                "content": text_parts,
            }),
        );
    }
    items
}

/// Render a [`ToolResultOutput`] into a Responses `function_call_output.output`
/// value. The wire's `output` is `string | content-part-array`, never a bare
/// JSON-value slot: `Text` / `ErrorText` pass through as a string, `Json` /
/// `ErrorJson` are **stringified** (the reference does `JSON.stringify`), and a
/// multimodal `Content` becomes a part array of `input_text` / `input_image` /
/// `input_file` — the same wire shapes a user message uses, so media survives
/// rather than being flattened to text. The error flag and tool name have no
/// slot here and are dropped.
/// <https://platform.openai.com/docs/api-reference/responses/create>
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
fn render_responses_tool_output(output: &ToolResultOutput) -> serde_json::Value {
    match output {
        // `output` is a string slot, not a JSON-value slot: stringify the value
        // (matching the reference's `JSON.stringify(output.value)`) rather than
        // emitting a bare object the wire would reject.
        ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
            serde_json::Value::String(value.to_string())
        }
        ToolResultOutput::Content { value } => serde_json::Value::Array(
            value
                .iter()
                .map(render_responses_tool_output_part)
                .collect(),
        ),
        other => serde_json::Value::String(other.to_provider_string()),
    }
}

/// Render one [`ToolResultContentPart`] into a Responses tool-output content
/// part. `text` → `input_text`; `image/*` media or an image file reference →
/// `input_image`; any other media or file reference → `input_file`. Media bytes
/// ride as a `data:` URL or a plain URL; a provider file reference rides as
/// `file_id`.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
fn render_responses_tool_output_part(part: &ToolResultContentPart) -> serde_json::Value {
    match part {
        ToolResultContentPart::Text { text } => {
            serde_json::json!({ "type": "input_text", "text": text })
        }
        ToolResultContentPart::Media { media_type, data } => {
            if media_type.starts_with("image/") {
                serde_json::json!({
                    "type": "input_image", "image_url": data.to_url(media_type)
                })
            } else {
                // The reference renders a `url`-form file as `file_url` and a
                // `data`-form file as `file_data`; `to_url` already collapses
                // inline bytes into a `data:` URL, so both land in `file_data`
                // here — a value the Responses wire accepts for either form.
                serde_json::json!({
                    "type": "input_file", "file_data": data.to_url(media_type)
                })
            }
        }
        ToolResultContentPart::FileId { media_type, id } => {
            let kind = match media_type {
                Some(mt) if mt.starts_with("image/") => "input_image",
                _ => "input_file",
            };
            serde_json::json!({ "type": kind, "file_id": id })
        }
    }
}

/// Parse a Responses `function_call_output.output` value into a canonical
/// [`ToolResultOutput`]. A string or bare JSON value uses the untyped mapping
/// (string → `Text`, else `Json`); a content-part array becomes the multimodal
/// `Content` variant, inverting [`render_responses_tool_output`].
/// <https://platform.openai.com/docs/api-reference/responses/create>
fn parse_responses_tool_output(value: &serde_json::Value) -> ToolResultOutput {
    match value {
        serde_json::Value::Array(parts) => ToolResultOutput::Content {
            value: parts
                .iter()
                .filter_map(parse_responses_tool_output_part)
                .collect(),
        },
        other => ToolResultOutput::from_untyped_value(other),
    }
}

/// Parse one Responses tool-output content part into a [`ToolResultContentPart`].
/// `input_text` → text; `input_image` / `input_file` carry either a `file_id`
/// (→ [`ToolResultContentPart::FileId`]) or an inline/URL payload
/// (→ [`ToolResultContentPart::Media`]).
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
fn parse_responses_tool_output_part(part: &serde_json::Value) -> Option<ToolResultContentPart> {
    match part.get("type").and_then(|t| t.as_str())? {
        "input_text" | "text" => Some(ToolResultContentPart::Text {
            text: part.get("text").and_then(|t| t.as_str())?.to_string(),
        }),
        "input_image" => {
            if let Some(id) = part.get("file_id").and_then(|f| f.as_str()) {
                return Some(ToolResultContentPart::FileId {
                    media_type: Some("image/*".to_string()),
                    id: id.to_string(),
                });
            }
            let url = part.get("image_url").and_then(|u| u.as_str())?;
            let (media_type, data) = DataContent::from_url(url);
            Some(ToolResultContentPart::Media {
                media_type: media_type.unwrap_or_else(|| "image/*".to_string()),
                data,
            })
        }
        "input_file" => {
            if let Some(id) = part.get("file_id").and_then(|f| f.as_str()) {
                return Some(ToolResultContentPart::FileId {
                    media_type: None,
                    id: id.to_string(),
                });
            }
            // The data form carries `file_data` (a `data:` URL); the url form
            // carries `file_url`. Either resolves through the shared parser.
            let payload = part
                .get("file_data")
                .or_else(|| part.get("file_url"))
                .and_then(|d| d.as_str())?;
            let (media_type, data) = DataContent::from_url(payload);
            Some(ToolResultContentPart::Media {
                media_type: media_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                data,
            })
        }
        _ => None,
    }
}

/// Render a canonical result into Responses `output` items.
fn render_output_items(result: &GenerateResult) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    let reasoning: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !reasoning.is_empty() {
        items.push(serde_json::json!({
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": reasoning }],
        }));
    }
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        // Re-attach citations as the `output_text` part's `annotations[]` (the
        // location `parse_response` lifts them from), collected from the result's
        // `Content::Source` parts rather than rendered per-part. The key is
        // omitted entirely when there are no sources (#454-5: never emit null /
        // gratuitous empty fields).
        let mut part = serde_json::json!({ "type": "output_text", "text": text });
        let annotations = render_responses_annotations(result);
        if !annotations.is_empty() {
            part["annotations"] = annotations.into();
        }
        items.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [part],
        }));
    }
    for c in &result.content {
        if let Content::ToolCall {
            id,
            name,
            arguments,
            provider_executed,
            dynamic,
            ..
        } = c
        {
            // A `dynamic` provider-executed MCP call recombines with its inline
            // result — the same-id `ToolResult` carrying the `{type:'call', …}`
            // body — back into ONE `mcp_call` item, so the inline output/error
            // round-trips. If no such paired result is present (e.g. an upstream
            // truncated the response after the call but before its inline result,
            // or the call was routed in from a non-Responses wire), the call is
            // degraded to a plain `function_call` below — it must NOT fall into
            // the `{name}_call` server-tool branch, which would emit an invalid
            // `mcp.<name>_call` item the Responses API does not define.
            if *dynamic
                && *provider_executed
                && let Some(item) = result
                    .content
                    .iter()
                    .filter_map(|r| match r {
                        Content::ToolResult {
                            call_id,
                            output,
                            dynamic: result_dynamic,
                            provider_metadata,
                            ..
                        } if *result_dynamic && call_id == id => {
                            render_mcp_call_item(call_id, provider_metadata, output)
                        }
                        _ => None,
                    })
                    .next()
            {
                items.push(item);
                continue;
            }
            // A client `local_shell` call reproduces its `local_shell_call` output
            // item, keyed by `call_id`, restoring the `action` payload and the
            // item `id` from `provider_metadata["openai"]["itemId"]`.
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
            if !*provider_executed
                && name == RESPONSES_LOCAL_SHELL_TOOL
                && let Some(item) = render_local_shell_call_item(c)
            {
                items.push(item);
                continue;
            }
            if *provider_executed && !*dynamic {
                // Reproduce the provider-executed server-tool output item on the
                // same wire. The Responses API names these items `<tool>_call`
                // (e.g. `web_search_call`) and keys them by item `id` rather than
                // `call_id`. `code_interpreter_call` carries its `code` /
                // `container_id` back out; the others have no echoed input.
                // A `dynamic` MCP call is deliberately excluded here: its native
                // shape is `mcp_call` (emitted only when its inline result is
                // paired, above), and `mcp.<name>_call` is not a valid Responses
                // item type — an unpaired one degrades to `function_call` instead.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/openai-responses-language-model.ts>
                let mut item = serde_json::Map::new();
                item.insert("type".into(), format!("{name}_call").into());
                item.insert("id".into(), id.clone().into());
                if name == "code_interpreter"
                    && let Ok(serde_json::Value::Object(input)) =
                        serde_json::from_str::<serde_json::Value>(arguments)
                {
                    if let Some(code) = input.get("code") {
                        item.insert("code".into(), code.clone());
                    }
                    if let Some(container) = input.get("containerId") {
                        item.insert("container_id".into(), container.clone());
                    }
                }
                items.push(serde_json::Value::Object(item));
            } else {
                // A client `function_call`, or a `dynamic` MCP call whose inline
                // result was not paired (handled above): both render as a valid
                // `function_call` item. The MCP tool name keeps its `mcp.` prefix
                // so a downstream consumer can still recover the bare tool name.
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
        }
    }
    // Reproduce a `mcp_approval_request` output item from each
    // `Content::ToolApprovalRequest`. The MCP server identity (`server_label` /
    // `name` / `arguments`) is restored from `provider_metadata["openai"]`. The
    // item `id` is the preserved raw `itemId` when the source carried one distinct
    // from the correlation key; otherwise it falls back to `approval_id`. When a
    // distinct `itemId` was preserved, `approval_request_id` is emitted alongside
    // (= `approval_id`) so the original two-id item round-trips byte-faithfully —
    // inverting the parse path. <https://platform.openai.com/docs/api-reference/responses/object>
    for c in &result.content {
        if let Content::ToolApprovalRequest {
            approval_id,
            provider_metadata,
            ..
        } = c
        {
            let mut item = serde_json::Map::new();
            item.insert("type".into(), "mcp_approval_request".into());
            let openai = provider_namespace(provider_metadata, PROVIDER_ID_OPENAI);
            let item_id = openai
                .and_then(|o| o.get("itemId"))
                .and_then(|v| v.as_str());
            item.insert("id".into(), item_id.unwrap_or(approval_id).into());
            // The correlation key is carried separately only when it differs from
            // the item id (i.e. the source had both); otherwise `id` alone conveys
            // it, matching the single-id items the parse path also accepts.
            if item_id.is_some() {
                item.insert("approval_request_id".into(), approval_id.clone().into());
            }
            if let Some(openai) = openai {
                if let Some(label) = openai.get("serverLabel") {
                    item.insert("server_label".into(), label.clone());
                }
                if let Some(name) = openai.get("name") {
                    item.insert("name".into(), name.clone());
                }
                if let Some(arguments) = openai.get("arguments") {
                    item.insert("arguments".into(), arguments.clone());
                }
            }
            items.push(serde_json::Value::Object(item));
        }
    }
    // Best-effort: a generated file becomes an image message item. The Responses
    // API has no standard output-image item, so this preserves the data rather
    // than dropping it. <https://platform.openai.com/docs/api-reference/responses>
    for c in &result.content {
        if let Content::File {
            media_type, data, ..
        } = c
        {
            items.push(serde_json::json!({
                "type": "message", "role": "assistant",
                "content": [{ "type": "output_image", "image_url": data.to_url(media_type) }],
            }));
        }
    }
    items
}

/// Render a Responses-shaped `usage` object that always emits the canonical
/// numeric fields and adds the cache subtree only when it'd carry signal.
fn render_responses_usage(usage: &Usage) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.total(),
        "output_tokens_details": { "reasoning_tokens": usage.reasoning_tokens },
    });
    if usage.cache_read_tokens > 0 {
        obj["input_tokens_details"] =
            serde_json::json!({ "cached_tokens": usage.cache_read_tokens });
    }
    obj
}

fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    let input = value.get("input_tokens").and_then(|v| v.as_u64())?;
    let output = value
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = value
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Responses' usage block exposes cached prompt tokens under
    // `input_tokens_details.cached_tokens`. Ref:
    // <https://platform.openai.com/docs/api-reference/responses/object>.
    let cache_read = value
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        reasoning_tokens: reasoning,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        web_search_count: 0,
        origin: UsageOrigin::ProviderReported,
        raw: Some(Box::new(value.clone())),
    })
}

#[cfg(test)]
#[test]
fn parse_usage_retains_provider_payload_and_origin() {
    use crate::language_model::types::UsageOrigin;

    let raw = serde_json::json!({
        "input_tokens": 12,
        "output_tokens": 4,
        "input_tokens_details": { "cached_tokens": 5 },
        "provider_extension": { "service_tier": "flex" }
    });

    let usage = parse_usage(&raw).expect("usage present");
    assert_eq!(usage.origin, UsageOrigin::ProviderReported);
    assert_eq!(usage.raw.as_deref(), Some(&raw));
}

// ===== streaming =====

/// Responses SSE decoder. Explicit state machine over the lifecycle
/// envelope. Tracks `item_id → call_id` so `function_call_arguments.delta`
/// events map back to the canonical tool-call id (#434).
#[derive(Default)]
struct ResponsesStreamDecoder {
    /// item_id → (call_id, tool_name) for in-flight function-call items.
    function_items: Vec<(String, String, String)>,
    /// Running citation count across `response.output_text.annotation.added`
    /// events, so synthesized [`Source`] ids stay unique within the stream
    /// (mirrors `next_index` on the non-streaming `parse_responses_annotations`).
    source_index: usize,
    /// Native identity introduced by `response.created`, when the upstream
    /// emits that optional lifecycle event. It must be unique, non-empty, and
    /// match the terminal response id.
    created_id: Option<String>,
    /// A successful terminal is buffered until EOF. This forces the production
    /// executor to consume and validate every later SSE event before exposing
    /// success to the pipeline, where a terminal otherwise stops polling.
    successful_terminal: Option<StreamPart>,
}

impl ResponsesStreamDecoder {
    fn call_for_item(&self, item_id: &str) -> Option<(String, String)> {
        self.function_items
            .iter()
            .find(|(id, _, _)| id == item_id)
            .map(|(_, call_id, name)| (call_id.clone(), name.clone()))
    }
}

/// Derive the block id for a [`StreamPart::TextStart`] / [`StreamPart::TextEnd`]
/// (and reasoning counterpart) from an `output_item.added` / `output_item.done`
/// event: prefer the upstream `item.id`, falling back to the event's
/// `output_index` when an upstream omits it. The id is used only to correlate a
/// start with its matching end; outbound encoders generate their own item ids,
/// so an exact value is not required for fidelity.
fn block_marker_id(item: &serde_json::Value, event: &serde_json::Value) -> String {
    if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
        return id.to_string();
    }
    event
        .get("output_index")
        .and_then(|i| i.as_u64())
        .map(|i| i.to_string())
        .unwrap_or_default()
}

impl StreamDecoder for ResponsesStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if self.successful_terminal.is_some() {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "Responses stream emitted non-empty data after its terminal".to_string(),
            });
        }
        let json: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        // When an explicit Responses `event:` name is present it must agree
        // with the JSON `type`. `message` is the SSE default used when the
        // event line is omitted, so body `type` remains authoritative there.
        let body_type = json.get("type").and_then(|t| t.as_str());
        let explicit_event = event.event.as_deref().filter(|name| *name != "message");
        if let (Some(header), Some(body)) = (explicit_event, body_type)
            && header != body
        {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: format!(
                    "Responses SSE event name '{header}' contradicts JSON type '{body}'"
                ),
            });
        }
        let event_type = body_type.or(explicit_event).unwrap_or_default();
        let mut parts = Vec::new();
        match event_type {
            "response.created" => {
                let id = json
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(|id| id.as_str())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| BitrouterError::UpstreamInvalidResponse {
                        message: "Responses response.created missing non-empty response id"
                            .to_string(),
                    })?;
                if self.created_id.is_some() {
                    return Err(BitrouterError::UpstreamInvalidResponse {
                        message: "Responses stream emitted duplicate response.created".to_string(),
                    });
                }
                self.created_id = Some(id.to_owned());
                parts.push(StreamPart::ResponseStarted {
                    id: id.to_owned(),
                    source_protocol: ApiProtocol::Responses,
                });
            }
            "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_text.done" => {}
            "response.output_item.added" => {
                // Streaming approval gap: a streamed `mcp_approval_request` item
                // would need a dedicated `StreamPart` variant to surface, but no
                // outbound encoder has a target for it (the other three protocols
                // carry no streaming approval frame), so adding one would be
                // unconstructed dead code. The non-streaming handshake
                // (`parse_response` / `render_output_items`) is the complete,
                // faithful round trip; a streamed approval item is simply not
                // emitted as a delta here. <https://platform.openai.com/docs/api-reference/responses-streaming>
                if let Some(item) = json.get("item") {
                    let item_type = item
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    let item_id = block_marker_id(item, &json);
                    match item_type {
                        // A `message` / `reasoning` item open is an explicit
                        // block boundary on this wire — surface it as a canonical
                        // start marker so a framing client encoder opens a fresh
                        // block (the merged-block fix: two distinct message items
                        // re-encode as two blocks, not one). The block id is the
                        // upstream item id, falling back to `output_index` if the
                        // upstream omitted it (used only for marker correlation).
                        "message" => parts.push(StreamPart::TextStart { id: item_id }),
                        "reasoning" => parts.push(StreamPart::ReasoningStart { id: item_id }),
                        "function_call" => {
                            // A function-call item frames itself via the
                            // `name`-bearing `ToolCallDelta` below — no separate
                            // start marker (see the `StreamPart` enum docs).
                            // Track the *real* upstream `item.id` (not the marker
                            // fallback) so `function_call_arguments.delta` — which
                            // carries `item_id` — correlates to this call (#434).
                            let real_item_id = item
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let call_id = item
                                .get("call_id")
                                .and_then(|i| i.as_str())
                                .unwrap_or(&real_item_id)
                                .to_string();
                            let name = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            self.function_items
                                .push((real_item_id, call_id.clone(), name.clone()));
                            parts.push(StreamPart::ToolCallDelta {
                                id: call_id,
                                name: Some(name),
                                arguments: String::new(),
                            });
                        }
                        // Streaming MCP gap: a streamed `mcp_call` item (a
                        // provider-executed remote MCP call with its inline
                        // result) is currently dropped on this delta path — like
                        // the `mcp_approval_request` gap documented above, it has
                        // no `StreamPart` variant to carry the call+inline-result
                        // pair, and surfacing it would require new cross-protocol
                        // `StreamPart` plumbing the other encoders cannot yet
                        // target. The non-streaming handshake (`parse_response` /
                        // `render_output_items`) is the complete, faithful round
                        // trip; a streamed `mcp_call` is simply not emitted as a
                        // delta here. Deferred, not lost.
                        // <https://platform.openai.com/docs/api-reference/responses-streaming>
                        "mcp_call" => {}
                        _ => {}
                    }
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                    parts.push(StreamPart::TextDelta {
                        text: delta.to_string(),
                    });
                }
            }
            "response.output_text.annotation.added" => {
                // A streamed citation mutates an open `output_text` item: the
                // single new annotation rides on the `annotation` field with the
                // same shape `parse_responses_annotations` reads. Lift it into a
                // `StreamPart::Source` (the client-side encoder re-attaches it at
                // the protocol's native citation location). Wrapped in a
                // one-element array to reuse the array parser; `source_index`
                // keeps synthesized ids unique across the stream.
                // <https://platform.openai.com/docs/api-reference/responses-streaming/response/output_text/annotation_added>
                if let Some(annotation) = json.get("annotation") {
                    let arr = serde_json::Value::Array(vec![annotation.clone()]);
                    for content in parse_responses_annotations(Some(&arr), &mut self.source_index) {
                        if let Content::Source { source, .. } = content {
                            parts.push(StreamPart::Source { source });
                        }
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                    parts.push(StreamPart::ReasoningDelta {
                        text: delta.to_string(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                // map item_id → call_id (#434); emit a single arguments delta
                let item_id = json
                    .get("item_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default();
                if let Some((call_id, _)) = self.call_for_item(item_id)
                    && let Some(delta) = json.get("delta").and_then(|d| d.as_str())
                {
                    parts.push(StreamPart::ToolCallDelta {
                        id: call_id,
                        name: None,
                        arguments: delta.to_string(),
                    });
                }
            }
            // the `.done` event repeats the full arguments — do NOT re-emit
            // them (would duplicate, #434).
            "response.function_call_arguments.done" => {}
            "response.output_item.done" => {
                // Close the matching text / reasoning block so the boundary
                // survives re-encoding (the merged-block fix). A `function_call`
                // item carries no end marker (its framing is the `name`-bearing
                // delta on open and the next item / terminal on close).
                if let Some(item) = json.get("item") {
                    let item_type = item
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default();
                    let item_id = block_marker_id(item, &json);
                    match item_type {
                        "message" => parts.push(StreamPart::TextEnd { id: item_id }),
                        "reasoning" => parts.push(StreamPart::ReasoningEnd {
                            id: item_id,
                            // Responses reasoning continuity (encrypted_content) is
                            // not carried on this slot; only the Anthropic Messages
                            // wire populates the thinking `signature`.
                            signature: None,
                        }),
                        _ => {}
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                // #454-2: the full `response` object is carried here..3:
                // map `response.completed` to the dedicated `ResponseCompleted`
                // part so the response id + status survive (a bare `Finish`
                // would lose them).
                let response = json.get("response");
                let expected_status = if event_type == "response.incomplete" {
                    "incomplete"
                } else {
                    "completed"
                };
                let actual_status = response
                    .and_then(|r| r.get("status"))
                    .and_then(|status| status.as_str());
                if actual_status != Some(expected_status) {
                    return Err(BitrouterError::UpstreamInvalidResponse {
                        message: format!(
                            "Responses terminal event '{event_type}' contradicts body status '{}'",
                            actual_status.unwrap_or("<missing>")
                        ),
                    });
                }
                let usage = response.and_then(|r| r.get("usage")).and_then(parse_usage);
                let response_output_commitment = response
                    .and_then(terminal_assistant_turn_commitment)
                    .map(|commitment| commitment.as_str().to_owned());
                let id = response
                    .and_then(|r| r.get("id"))
                    .and_then(|i| i.as_str())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| BitrouterError::UpstreamInvalidResponse {
                        message: "Responses terminal event missing non-empty response id"
                            .to_string(),
                    })?
                    .to_string();
                if self
                    .created_id
                    .as_deref()
                    .is_some_and(|created_id| created_id != id)
                {
                    return Err(BitrouterError::UpstreamInvalidResponse {
                        message: "Responses terminal id does not match response.created id"
                            .to_string(),
                    });
                }
                self.successful_terminal = Some(StreamPart::ResponseCompleted {
                    id,
                    source_protocol: ApiProtocol::Responses,
                    status: expected_status.to_string(),
                    usage,
                    response_output_commitment,
                });
            }
            "error" | "response.failed" => {
                // Map OpenAI's `error.type` to an HTTP status so 4xx upstream
                // errors don't always 502 here (which would trigger fallback
                // retries). Spec:
                // <https://platform.openai.com/docs/guides/error-codes>.
                let error_obj = json
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| json.get("error"));
                let err_type = error_obj
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let msg = error_obj
                    .and_then(|e| e.get("message"))
                    .or_else(|| json.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("responses stream error");
                if openai_policy_violation(error_obj, msg) {
                    return Err(BitrouterError::UpstreamPolicyViolation {
                        message: msg.to_string(),
                    });
                }
                return Err(BitrouterError::Upstream {
                    status: openai_error_status(err_type),
                    message: msg.to_string(),
                });
            }
            // genuinely unknown, forward-compatible event — ignore (#432)
            _ => {
                tracing::debug!("ignoring unknown responses stream event");
            }
        }
        Ok(parts)
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>> {
        self.successful_terminal
            .take()
            .map(|terminal| vec![terminal])
            .ok_or_else(|| BitrouterError::UpstreamInvalidResponse {
                message: "Responses stream ended before a valid terminal event".to_string(),
            })
    }
}

/// Responses SSE encoder. Emits the complete lifecycle envelope:
///
/// ```text
/// response.created
/// (per output item, in arrival order)
///   response.output_item.added
///   response.content_part.added            (for `message` / `reasoning` items)
///   response.<text|reasoning_text|...>.delta  (one per chunk)
///   response.<text|reasoning_text|...>.done
///   response.content_part.done
///   response.output_item.done
/// response.completed
/// ```
///
/// Strict clients (e.g. Codex CLI) silently discard deltas whose
/// `output_index` was never opened via `response.output_item.added` +
/// `response.content_part.added`, then time out the stream. Emitting
/// the full lifecycle is required for Responses-API compatibility —
/// this mirrors the v0 emitter in
/// `bitrouter-core/src/api/openai/responses/convert.rs::StreamConverter`.
///
/// Every event carries a monotonically increasing `sequence_number`.
/// Never emits `[DONE]` (#454-2).
struct ResponsesStreamEncoder {
    response_id: Result<String>,
    model: String,
    seq: u64,
    created: bool,
    /// Next free `output_index` to hand to a newly-opened item.
    /// Incremented every time we open + close one.
    next_output_index: u64,
    /// State of the in-flight reasoning item, if any. Reasoning is
    /// closed before a message item opens (codex and the spec both
    /// require items to not interleave their deltas).
    reasoning_item: Option<ItemState>,
    /// State of the in-flight message item, if any. Closes on the
    /// first `ToolCallDelta` (new function-call item starts) or on
    /// `Finish` / `ResponseCompleted`.
    text_item: Option<ItemState>,
    /// State of the in-flight function-call item, if any. Closes on the
    /// next item of any kind, or on the terminal part.
    tool_item: Option<ToolItemState>,
    /// Every closed output item, as its final JSON, in emission order.
    /// Replayed into `response.completed.response.output` so the
    /// terminal envelope mirrors the non-streaming Responses object —
    /// Codex CLI reconstructs the assistant turn from this array, so an
    /// empty `output` renders as a blank turn (the symptom this fixes).
    completed_items: Vec<serde_json::Value>,
    /// Running count of `response.output_text.annotation.added` events emitted,
    /// supplying the monotonically increasing `annotation_index` each event
    /// carries.
    annotation_index: u64,
    /// A buffered [`StreamPart::ServerToolCall`] awaiting its result, so the
    /// pair emits as one `mcp_call` output item (OpenAI carries arguments +
    /// output on a single item).
    pending_mcp_call: Option<PendingMcpCall>,
}

/// A buffered router tool call awaiting its [`StreamPart::ServerToolResult`].
struct PendingMcpCall {
    id: String,
    name: String,
    arguments: String,
    server_name: Option<String>,
}

/// Map a router tool-result output to an OpenAI `mcp_call` `output` string and
/// optional `error` string.
fn mcp_call_output(output: &ToolResultOutput) -> (String, Option<String>) {
    match output {
        ToolResultOutput::Text { value } => (value.clone(), None),
        ToolResultOutput::ErrorText { value } => (String::new(), Some(value.clone())),
        ToolResultOutput::Json { value } => (value.to_string(), None),
        ToolResultOutput::ErrorJson { value } => (String::new(), Some(value.to_string())),
        ToolResultOutput::Content { value } => {
            let text = value
                .iter()
                .filter_map(|p| match p {
                    ToolResultContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (text, None)
        }
        ToolResultOutput::ExecutionDenied { reason } => (
            String::new(),
            Some(format!(
                "execution denied: {}",
                reason.as_deref().unwrap_or("")
            )),
        ),
    }
}

/// Tracking state for one open message / reasoning item.
struct ItemState {
    item_id: String,
    output_index: u64,
    /// Accumulated text — written into the `*.done` event so the
    /// final envelope carries the full body, matching the
    /// non-streaming Responses object.
    accumulated_text: String,
}

/// Tracking state for one open function-call item.
struct ToolItemState {
    /// The upstream tool-call id (`call_id`).
    call_id: String,
    item_id: String,
    output_index: u64,
    tool_name: String,
    /// Accumulated argument JSON fragments.
    accumulated_args: String,
}

impl ResponsesStreamEncoder {
    fn ev(&mut self, type_name: &str, mut data: serde_json::Value) -> SseFrame {
        let seq = self.seq;
        self.seq += 1;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("type".into(), type_name.into());
            obj.insert("sequence_number".into(), seq.into());
        }
        SseFrame::Event {
            event: Some(type_name.to_string()),
            data: data.to_string(),
        }
    }

    fn ensure_created(&mut self, frames: &mut Vec<SseFrame>) -> Result<()> {
        if !self.created {
            let response_id = self.response_id.clone()?;
            self.created = true;
            let response = serde_json::json!({
                "id": response_id,
                "object": "response",
                "model": self.model,
                "status": "in_progress",
                "output": [],
            });
            // The Responses stream opens with `response.created`
            // *then* `response.in_progress`. Codex CLI's state machine
            // waits for `in_progress` before it starts consuming output
            // items — emit both.
            frames.push(self.ev(
                "response.created",
                serde_json::json!({ "response": response.clone() }),
            ));
            frames.push(self.ev(
                "response.in_progress",
                serde_json::json!({ "response": response }),
            ));
        }
        Ok(())
    }

    /// Allocate the next `output_index`, opening a fresh slot. Returns
    /// the index that should be set on the item being opened.
    fn allocate_output_index(&mut self) -> u64 {
        let idx = self.next_output_index;
        self.next_output_index += 1;
        idx
    }

    /// Open a reasoning item: `output_item.added` (type=reasoning) +
    /// `content_part.added` (type=reasoning_text). Idempotent — closes
    /// any open message / tool item first so items never interleave.
    fn open_reasoning_item(&mut self, frames: &mut Vec<SseFrame>) {
        if self.reasoning_item.is_some() {
            return;
        }
        self.close_text_item(frames);
        self.close_tool_item(frames);
        let output_index = self.allocate_output_index();
        let item_id = format!("rs_{}", uuid::Uuid::new_v4());
        frames.push(self.ev(
            "response.output_item.added",
            serde_json::json!({
                "output_index": output_index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                    "content": [],
                    "status": "in_progress",
                },
            }),
        ));
        frames.push(self.ev(
            "response.content_part.added",
            serde_json::json!({
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "reasoning_text", "text": "" },
            }),
        ));
        self.reasoning_item = Some(ItemState {
            item_id,
            output_index,
            accumulated_text: String::new(),
        });
    }

    /// Close the open reasoning item, if any: `reasoning_text.done` +
    /// `content_part.done` + `output_item.done`.
    fn close_reasoning_item(&mut self, frames: &mut Vec<SseFrame>) {
        let Some(state) = self.reasoning_item.take() else {
            return;
        };
        let final_text = state.accumulated_text;
        frames.push(self.ev(
            "response.reasoning_text.done",
            serde_json::json!({
                "item_id": state.item_id,
                "output_index": state.output_index,
                "content_index": 0,
                "text": final_text,
            }),
        ));
        frames.push(self.ev(
            "response.content_part.done",
            serde_json::json!({
                "item_id": state.item_id,
                "output_index": state.output_index,
                "content_index": 0,
                "part": { "type": "reasoning_text", "text": final_text },
            }),
        ));
        let item = serde_json::json!({
            "type": "reasoning",
            "id": state.item_id,
            "summary": [],
            "content": [{ "type": "reasoning_text", "text": final_text }],
            "status": "completed",
        });
        frames.push(self.ev(
            "response.output_item.done",
            serde_json::json!({
                "output_index": state.output_index,
                "item": item.clone(),
            }),
        ));
        self.completed_items.push(item);
    }

    /// Force-open a *fresh* reasoning item for an explicit
    /// [`StreamPart::ReasoningStart`]. Unlike [`Self::open_reasoning_item`] this
    /// first closes any reasoning item already open, so two distinct upstream
    /// `reasoning` items re-encode as two `output_item.added`s rather than one
    /// merged block (the merged-block fix on this wire). Closing the prior item
    /// before reopening also re-runs the cross-kind close inside
    /// `open_reasoning_item`, so message / tool items never interleave.
    /// <https://platform.openai.com/docs/api-reference/responses-streaming>
    fn open_fresh_reasoning_item(&mut self, frames: &mut Vec<SseFrame>) {
        self.close_reasoning_item(frames);
        self.open_reasoning_item(frames);
    }

    /// Open a message item: `output_item.added` (type=message) +
    /// `content_part.added` (type=output_text). Idempotent — closes any
    /// open reasoning / tool item first so items never interleave.
    fn open_text_item(&mut self, frames: &mut Vec<SseFrame>) {
        if self.text_item.is_some() {
            return;
        }
        self.close_reasoning_item(frames);
        self.close_tool_item(frames);
        let output_index = self.allocate_output_index();
        let item_id = format!("msg_{}", uuid::Uuid::new_v4());
        frames.push(self.ev(
            "response.output_item.added",
            serde_json::json!({
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": item_id,
                    "role": "assistant",
                    "content": [],
                    "status": "in_progress",
                },
            }),
        ));
        frames.push(self.ev(
            "response.content_part.added",
            serde_json::json!({
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": "" },
            }),
        ));
        self.text_item = Some(ItemState {
            item_id,
            output_index,
            accumulated_text: String::new(),
        });
    }

    /// Close the open message item, if any: `output_text.done` +
    /// `content_part.done` + `output_item.done`.
    fn close_text_item(&mut self, frames: &mut Vec<SseFrame>) {
        let Some(state) = self.text_item.take() else {
            return;
        };
        let final_text = state.accumulated_text;
        frames.push(self.ev(
            "response.output_text.done",
            serde_json::json!({
                "item_id": state.item_id,
                "output_index": state.output_index,
                "content_index": 0,
                "text": final_text,
            }),
        ));
        frames.push(self.ev(
            "response.content_part.done",
            serde_json::json!({
                "item_id": state.item_id,
                "output_index": state.output_index,
                "content_index": 0,
                "part": { "type": "output_text", "text": final_text },
            }),
        ));
        let item = serde_json::json!({
            "type": "message",
            "id": state.item_id,
            "role": "assistant",
            "content": [{ "type": "output_text", "text": final_text }],
            "status": "completed",
        });
        frames.push(self.ev(
            "response.output_item.done",
            serde_json::json!({
                "output_index": state.output_index,
                "item": item.clone(),
            }),
        ));
        self.completed_items.push(item);
    }

    /// Force-open a *fresh* message item for an explicit
    /// [`StreamPart::TextStart`]. Unlike [`Self::open_text_item`] this first
    /// closes any message item already open, so two distinct upstream `message`
    /// items re-encode as two `output_item.added`s rather than one merged block
    /// (the merged-block fix on this wire). Closing the prior item before
    /// reopening also re-runs the cross-kind close inside `open_text_item`, so
    /// reasoning / tool items never interleave.
    /// <https://platform.openai.com/docs/api-reference/responses-streaming>
    fn open_fresh_text_item(&mut self, frames: &mut Vec<SseFrame>) {
        self.close_text_item(frames);
        self.open_text_item(frames);
    }

    /// Open a function-call item: `output_item.added` (type=function_call).
    /// Closes any other open item first. Idempotent for the same call.
    fn open_tool_item(&mut self, frames: &mut Vec<SseFrame>, call_id: &str, name: &str) {
        self.close_reasoning_item(frames);
        self.close_text_item(frames);
        self.close_tool_item(frames);
        let output_index = self.allocate_output_index();
        let item_id = format!("fc_{}", uuid::Uuid::new_v4());
        frames.push(self.ev(
            "response.output_item.added",
            serde_json::json!({
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                },
            }),
        ));
        self.tool_item = Some(ToolItemState {
            call_id: call_id.to_string(),
            item_id,
            output_index,
            tool_name: name.to_string(),
            accumulated_args: String::new(),
        });
    }

    /// Close the open function-call item, if any:
    /// `function_call_arguments.done` + `output_item.done`.
    fn close_tool_item(&mut self, frames: &mut Vec<SseFrame>) {
        let Some(state) = self.tool_item.take() else {
            return;
        };
        let final_args = state.accumulated_args;
        frames.push(self.ev(
            "response.function_call_arguments.done",
            serde_json::json!({
                "item_id": state.item_id,
                "output_index": state.output_index,
                "arguments": final_args,
            }),
        ));
        let item = serde_json::json!({
            "type": "function_call",
            "id": state.item_id,
            "call_id": state.call_id,
            "name": state.tool_name,
            "arguments": final_args,
            "status": "completed",
        });
        frames.push(self.ev(
            "response.output_item.done",
            serde_json::json!({
                "output_index": state.output_index,
                "item": item.clone(),
            }),
        ));
        self.completed_items.push(item);
    }

    /// Emit the terminal lifecycle frame — `response.completed` (or
    /// `response.incomplete`), carrying the full `response` object (#454-2).
    /// Shared by the `Finish` and `ResponseCompleted` encode arms.
    /// Closes any still-open reasoning / message / tool items first, and
    /// replays every closed item into `response.output` so the terminal
    /// envelope mirrors the non-streaming Responses object — Codex CLI
    /// reconstructs the assistant turn from this array.
    fn emit_terminal(
        &mut self,
        frames: &mut Vec<SseFrame>,
        status: &str,
        response_id: &str,
        usage: Option<Usage>,
    ) {
        self.close_reasoning_item(frames);
        self.close_text_item(frames);
        self.close_tool_item(frames);
        let event_name = match status {
            "completed" => "response.completed",
            "incomplete" => "response.incomplete",
            _ => "response.failed",
        };
        let mut response = serde_json::json!({
            "id": response_id,
            "object": "response",
            "model": self.model,
            "status": status,
            "output": std::mem::take(&mut self.completed_items),
        });
        if let Some(u) = usage {
            // #454-5: numeric, never null; absent stays absent.
            response["usage"] = serde_json::json!({
                "input_tokens": u.prompt_tokens,
                "output_tokens": u.completion_tokens,
                "total_tokens": u.total(),
                "output_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
            });
        }
        frames.push(self.ev(event_name, serde_json::json!({ "response": response })));
    }
}

impl StreamEncoder for ResponsesStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        self.ensure_created(&mut frames)?;
        match part {
            StreamPart::File { .. } => {
                // Responses streaming surfaces generated files on the
                // non-streaming path; no inline file-output delta is emitted here.
            }
            StreamPart::TextStart { .. } => {
                // Explicit block boundary from a block-framed upstream — open a
                // *fresh* message item so two distinct upstream text blocks
                // re-encode as two `output_item.added`s, not one merged block.
                self.open_fresh_text_item(&mut frames);
            }
            StreamPart::ReasoningStart { .. } => {
                self.open_fresh_reasoning_item(&mut frames);
            }
            StreamPart::TextEnd { .. } => {
                // Close the message item the matching start opened, emitting its
                // `output_text.done` + `content_part.done` + `output_item.done`.
                // A no-op if nothing is open, so a stray end is harmless.
                self.close_text_item(&mut frames);
            }
            StreamPart::ReasoningEnd { .. } => {
                // Close the reasoning item the matching start opened (its
                // `reasoning_text.done` + `content_part.done` + `output_item.done`).
                self.close_reasoning_item(&mut frames);
            }
            StreamPart::TextDelta { text } => {
                // `open_text_item` closes any open reasoning / tool item
                // first — items never interleave their deltas.
                self.open_text_item(&mut frames);
                let state = self.text_item.as_mut().expect("text item just opened");
                state.accumulated_text.push_str(text);
                let (item_id, output_index) = (state.item_id.clone(), state.output_index);
                frames.push(self.ev(
                    "response.output_text.delta",
                    serde_json::json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
            }
            StreamPart::ReasoningDelta { text } => {
                self.open_reasoning_item(&mut frames);
                let state = self
                    .reasoning_item
                    .as_mut()
                    .expect("reasoning item just opened");
                state.accumulated_text.push_str(text);
                let (item_id, output_index) = (state.item_id.clone(), state.output_index);
                frames.push(self.ev(
                    "response.reasoning_text.delta",
                    serde_json::json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                // A delta carrying a *non-empty* `name` starts a new
                // function-call item. `open_tool_item` closes any previously-open
                // item (reasoning / message / prior tool) first, so each tool
                // call lands in its own output slot. An empty name is NOT a new
                // call: some upstreams re-send `name:""` on every
                // argument-continuation delta, and treating `Some("")` as a new
                // call fragments one tool call into one broken item per delta
                // (empty name + partial args) — which Codex rejects as
                // "unsupported call" / unparsable arguments.
                if let Some(name) = name.as_deref().filter(|n| !n.is_empty()) {
                    self.open_tool_item(&mut frames, id, name);
                }
                if !arguments.is_empty() {
                    // Append to the in-flight tool item. If a stray
                    // arguments delta arrives with no opened item (the
                    // upstream omitted the name), open one with an empty
                    // name rather than dropping the delta.
                    if self.tool_item.is_none() {
                        self.open_tool_item(&mut frames, id, "");
                    }
                    let state = self.tool_item.as_mut().expect("tool item just opened");
                    state.accumulated_args.push_str(arguments);
                    let (item_id, output_index) = (state.item_id.clone(), state.output_index);
                    frames.push(self.ev(
                        "response.function_call_arguments.delta",
                        serde_json::json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": arguments,
                        }),
                    ));
                }
            }
            // Buffer a router-executed call; the matching result completes it
            // into one `mcp_call` output item.
            StreamPart::ServerToolCall {
                id,
                name,
                arguments,
                server_name,
                ..
            } => {
                self.pending_mcp_call = Some(PendingMcpCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    server_name: server_name.clone(),
                });
            }
            // Emit the complete `mcp_call` output item (arguments + output on one
            // item), pairing it with the buffered call.
            StreamPart::ServerToolResult {
                call_id, output, ..
            } => {
                self.close_reasoning_item(&mut frames);
                self.close_text_item(&mut frames);
                self.close_tool_item(&mut frames);
                let pending = self.pending_mcp_call.take();
                let pending = pending.filter(|p| &p.id == call_id);
                let name = pending.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                let arguments = pending
                    .as_ref()
                    .map(|p| p.arguments.clone())
                    .unwrap_or_default();
                let server_label = pending.as_ref().and_then(|p| p.server_name.clone());
                let (out_str, err) = mcp_call_output(output);
                let output_index = self.allocate_output_index();
                let item_id = format!("mcp_{}", uuid::Uuid::new_v4());
                let mut item = serde_json::json!({
                    "type": "mcp_call",
                    "id": item_id,
                    "name": name,
                    "arguments": arguments,
                    "output": out_str,
                    "status": "completed",
                });
                if let Some(server) = &server_label {
                    item["server_label"] = serde_json::Value::String(server.clone());
                }
                if let Some(e) = &err {
                    item["error"] = serde_json::Value::String(e.clone());
                }
                frames.push(self.ev(
                    "response.output_item.added",
                    serde_json::json!({ "output_index": output_index, "item": item.clone() }),
                ));
                frames.push(self.ev(
                    "response.output_item.done",
                    serde_json::json!({ "output_index": output_index, "item": item.clone() }),
                ));
                self.completed_items.push(item);
            }
            StreamPart::Source { source } => {
                // A citation is a `response.output_text.annotation.added` event
                // that mutates an `output_text` item. Ensure a text item is open
                // (a cross-protocol source may arrive before any text delta), so
                // the annotation has a valid item to attach to, then emit it
                // referencing that item. This mirrors the decode arm above and
                // the non-streaming `render_responses_annotations`.
                // <https://platform.openai.com/docs/api-reference/responses-streaming/response/output_text/annotation_added>
                self.open_text_item(&mut frames);
                if let Some(state) = self.text_item.as_ref() {
                    let (item_id, output_index) = (state.item_id.clone(), state.output_index);
                    let annotation_index = self.annotation_index;
                    self.annotation_index += 1;
                    let annotation = render_source_annotation(source);
                    frames.push(self.ev(
                        "response.output_text.annotation.added",
                        serde_json::json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "annotation_index": annotation_index,
                            "annotation": annotation,
                        }),
                    ));
                }
            }
            StreamPart::Usage { .. } => {}
            StreamPart::ResponseStarted { .. } => {
                // Provider response identity is request-local continuation
                // metadata. The public lifecycle remains on the gateway id.
            }
            StreamPart::Finish { reason } => {
                // A bare `Finish` (e.g. inbound was Chat Completions / Messages /
                // Generate Content) — synthesise the terminal envelope from the reason.
                let status = match reason {
                    FinishReason::Length => "incomplete",
                    FinishReason::Error(_) => "failed",
                    _ => "completed",
                };
                let response_id = self.response_id.clone()?;
                self.emit_terminal(&mut frames, status, &response_id, None);
            }
            StreamPart::ResponseCompleted { status, usage, .. } => {
                let response_id = self.response_id.clone()?;
                self.emit_terminal(&mut frames, status, &response_id, usage.clone());
            }
        }
        Ok(frames)
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Responses surfaces a mid-stream error as a `response.failed` event,
        // carrying the full response object with an `error` (#454-2 envelope).
        let response_id = self
            .response_id
            .clone()
            .unwrap_or_else(|_| "brc_aW52YWxpZC1yZXF1ZXN0LWlk".to_owned());
        let response = serde_json::json!({
            "id": response_id,
            "object": "response",
            "model": self.model,
            "status": "failed",
            "error": { "code": "upstream_error", "message": message },
        });
        vec![self.ev(
            "response.failed",
            serde_json::json!({ "response": response }),
        )]
    }

    fn encode_bitrouter_error(&mut self, error: &BitrouterError) -> Vec<SseFrame> {
        let response_id = self
            .response_id
            .clone()
            .unwrap_or_else(|_| "brc_aW52YWxpZC1yZXF1ZXN0LWlk".to_owned());
        let response = serde_json::json!({
            "id": response_id,
            "object": "response",
            "model": self.model,
            "status": "failed",
            "error": { "code": error.error_code(), "message": error.public_message() },
        });
        vec![self.ev(
            "response.failed",
            serde_json::json!({ "response": response }),
        )]
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        // #454-2: Responses never emits a `[DONE]` sentinel.
        Ok(Vec::new())
    }
}
