//! Protocol adapters for the `language_model` protocol.
//!
//! Four wire protocols — Anthropic Messages, OpenAI Chat Completions, OpenAI
//! Responses, Google Generative AI — each convert to/from the canonical
//! internal representation ([`Prompt`] / [`GenerateResult`] / [`StreamPart`]).
//! Any inbound protocol can be paired with any outbound protocol (the 4×4
//! conversion matrix).
//!
//! Design rules (008 Phase 2 / 005 §10):
//! - streaming parsing is an **explicit state machine**, never a catch-all
//!   `_ =>` arm that silently swallows variants;
//! - wire types omit absent fields entirely (`skip_serializing_if`), never
//!   emit JSON `null` (v0 #454-5);
//! - role mapping is **total** — an unknown role is an error, not a silent
//!   downgrade to `user` (v0 #454-4).
//!
//! All four adapters are compiled in unconditionally (no feature gate).

use crate::error::Result;
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{ApiProtocol, GenerateResult, Prompt, StreamPart};

pub mod anthropic;
pub mod google;
pub mod openai_chat;
pub mod openai_responses;

#[cfg(test)]
mod tests;

/// A parsed Server-Sent-Events event from an upstream stream.
#[derive(Debug, Clone, Default)]
pub struct SseEvent {
    /// The `event:` field, if the provider sets one (Anthropic does, OpenAI
    /// Chat does not).
    pub event: Option<String>,
    /// The `data:` payload — the raw (usually JSON) string.
    pub data: String,
}

/// Converts between one wire protocol and the canonical internal
/// representation, in both directions.
pub trait ProtocolAdapter: Send + Sync {
    /// The wire protocol this adapter speaks.
    fn protocol(&self) -> ApiProtocol;

    /// Inbound: parse a client/provider request body into a canonical [`Prompt`].
    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt>;

    /// Outbound: render a canonical [`Prompt`] into this protocol's request body.
    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value>;

    /// Inbound: parse a provider response body into a canonical [`GenerateResult`].
    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult>;

    /// Outbound: render a canonical [`GenerateResult`] into this protocol's
    /// response body. `prompt` and `request_id` supply envelope fields the
    /// protocol requires (model echo, response id, …).
    fn render_response(
        &self,
        result: &GenerateResult,
        prompt: &Prompt,
        request_id: &str,
    ) -> Result<serde_json::Value>;

    /// A fresh stateful decoder turning this protocol's SSE events into
    /// canonical [`StreamPart`]s.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// A fresh stateful encoder turning canonical [`StreamPart`]s into this
    /// protocol's SSE frames.
    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder>;
}

/// Stateful decoder: upstream SSE events → canonical stream parts. Streaming
/// protocols are explicit state machines (005 §10.3).
pub trait StreamDecoder: Send {
    /// Feed one SSE event; emit zero or more canonical parts.
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>>;

    /// Called once at stream end; flush any buffered state.
    fn finish(&mut self) -> Result<Vec<StreamPart>> {
        Ok(Vec::new())
    }
}

/// Stateful encoder: canonical stream parts → client SSE frames.
pub trait StreamEncoder: Send {
    /// Encode one canonical part into zero or more SSE frames.
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>>;

    /// Encode a mid-stream error into a **protocol-shaped terminal error
    /// frame** the client will actually recognise. The default emits an SSE
    /// comment (ignorable); each protocol adapter overrides this to emit its
    /// real error event (Anthropic `error`, OpenAI error chunk, Responses
    /// `response.failed`). After this the stream stops.
    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        vec![SseFrame::Comment(format!("error: {message}"))]
    }

    /// Called once at clean stream end; emit any trailing frames (e.g. the
    /// OpenAI `[DONE]` sentinel — note Responses must **not** emit it, #454-2).
    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        Ok(Vec::new())
    }
}

/// The adapter for a given wire protocol.
pub fn adapter_for(protocol: ApiProtocol) -> Box<dyn ProtocolAdapter> {
    match protocol {
        ApiProtocol::Openai => Box::new(openai_chat::OpenAiChatAdapter),
        ApiProtocol::Anthropic => Box::new(anthropic::AnthropicAdapter),
        ApiProtocol::Responses => Box::new(openai_responses::OpenAiResponsesAdapter),
        ApiProtocol::Google => Box::new(google::GoogleAdapter),
    }
}

/// Strip ANSI escape sequences and control bytes from a model name before it
/// is used for routing. v0 #276: an escape sequence such as `\x1b[1m` leaking
/// into the model name produced a 500; after sanitising, an unknown model is a
/// clean 404. The result is trimmed; empty input stays empty (the router then
/// 404s).
pub fn sanitize_model_name(raw: &str) -> String {
    // First drop CSI escape sequences: ESC `[` … final byte in `@`..=`~`.
    let mut without_csi = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // consume an optional `[` and everything up to the final byte
            if chars.peek() == Some(&'[') {
                chars.next();
                for inner in chars.by_ref() {
                    if ('@'..='~').contains(&inner) {
                        break;
                    }
                }
            }
            continue;
        }
        without_csi.push(c);
    }
    // Then drop any remaining control / delete bytes.
    without_csi
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .collect::<String>()
        .trim()
        .to_string()
}

/// Wrap a serde error with the target type name and a truncated body preview —
/// v0 #367 → #391: deserialisation failures must be diagnosable, not opaque.
pub fn describe_deser_error(
    type_name: &str,
    err: &serde_json::Error,
    body: &serde_json::Value,
) -> crate::error::BitrouterError {
    let preview = {
        let s = body.to_string();
        // Take up to 240 chars (not bytes) — slicing at a byte index would
        // panic if the cut fell inside a multi-byte UTF-8 sequence (the body
        // is attacker-controlled JSON, see regression for non-ASCII inputs).
        const PREVIEW_CHARS: usize = 240;
        let truncated: String = s.chars().take(PREVIEW_CHARS).collect();
        if truncated.chars().count() < s.chars().count() {
            format!("{truncated}…")
        } else {
            truncated
        }
    };
    crate::error::BitrouterError::bad_request(format!(
        "failed to deserialize {type_name}: {err} (at line {}, column {}); body preview: {preview}",
        err.line(),
        err.column(),
    ))
}
