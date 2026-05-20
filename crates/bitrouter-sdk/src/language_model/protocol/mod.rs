//! Protocol adapters for the `language_model` protocol.
//!
//! Four built-in wire protocols — OpenAI Chat Completions, OpenAI Responses,
//! Anthropic Messages, Google Generative AI — each convert to/from the
//! canonical internal representation ([`Prompt`] / [`GenerateResult`] /
//! [`StreamPart`]). Any inbound protocol can be paired with any outbound
//! protocol (the 4×4 conversion matrix).
//!
//! ## The three traits
//!
//! Each direction is its own trait so a provider can implement only the half
//! that applies:
//!
//! - [`InboundAdapter`] — parse a request body that arrived from a client
//!   and render the canonical result back. Used by the HTTP server.
//! - [`OutboundAdapter`] — render a canonical request into an upstream
//!   provider's wire format and parse its response back. Used by the
//!   executor.
//! - [`Transport`] — URL shape + auth scheme for one outbound provider.
//!   Bundled with an `OutboundAdapter` in the executor's
//!   [`OutboundDispatch`] registry.
//!
//! The four built-in protocols implement all three.
//!
//! ## Design rules
//!
//! - streaming parsing is an **explicit state machine**, never a catch-all
//!   `_ =>` arm that silently swallows variants;
//! - wire types omit absent fields entirely (`skip_serializing_if`), never
//!   emit JSON `null` (v0 #454-5);
//! - role mapping is **total** — an unknown role is an error, not a silent
//!   downgrade to `user` (v0 #454-4).
//!
//! ## Adding an outbound-only provider
//!
//! Platform-specific providers (AWS Bedrock, Azure OpenAI, Vertex AI, …) need
//! their own wire format + auth + URL conventions but the SDK never serves
//! their protocol back to clients. To add one — typically in its own crate
//! (`bitrouter-bedrock`, `bitrouter-azure-openai`, …):
//!
//! 1. Pick a unique protocol name and use
//!    [`ApiProtocol::Custom`]`("bedrock-claude".into())` to identify it.
//! 2. Implement [`OutboundAdapter`] for the wire-format conversion. Its
//!    `protocol()` method returns the same `ApiProtocol::Custom` value.
//! 3. Implement [`Transport`] for the URL shape and authentication scheme
//!    (e.g. AWS SigV4 signing for Bedrock).
//! 4. Register both with [`OutboundDispatch`] before building the executor:
//!
//! ```ignore
//! use std::sync::Arc;
//! use bitrouter_sdk::language_model::{HttpExecutor, HttpTimeouts};
//! use bitrouter_sdk::language_model::protocol::OutboundDispatch;
//!
//! let mut dispatch = OutboundDispatch::builtin();
//! dispatch.register(
//!     Arc::new(BedrockClaudeAdapter::new()),
//!     Arc::new(BedrockTransport::new(region, credentials)),
//! );
//! let executor = HttpExecutor::with_dispatch(HttpTimeouts::default(), dispatch)?;
//! # Ok::<(), bitrouter_sdk::BitrouterError>(())
//! ```
//!
//! Clients still call BitRouter using one of the four built-in inbound
//! protocols; the routing table directs the request to a target whose
//! `api_protocol` is `ApiProtocol::Custom("bedrock-claude")`, and the
//! executor dispatches through the registered adapter + transport.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, GenerateResult, Prompt, RoutingTarget, StreamPart,
};

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

/// **Inbound** conversion: parse a request body that arrived from a client
/// and render the canonical result back. Used by the HTTP server to terminate
/// one inbound wire protocol.
///
/// Stateless. The four built-in adapters are zero-sized.
pub trait InboundAdapter: Send + Sync {
    /// The wire protocol this adapter speaks.
    fn protocol(&self) -> ApiProtocol;

    /// Parse a client request body into a canonical [`Prompt`].
    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt>;

    /// Render a canonical [`GenerateResult`] into this protocol's response
    /// body. `prompt` and `request_id` supply envelope fields the protocol
    /// requires (model echo, response id, …).
    fn render_response(
        &self,
        result: &GenerateResult,
        prompt: &Prompt,
        request_id: &str,
    ) -> Result<serde_json::Value>;

    /// A fresh stateful encoder turning canonical [`StreamPart`]s into this
    /// protocol's SSE frames.
    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder>;
}

/// **Outbound** conversion: render a canonical request into an upstream
/// provider's wire format and parse its response back. Used by the executor
/// when calling an upstream provider.
///
/// Stateless. Pair with a [`Transport`] in [`OutboundDispatch`].
pub trait OutboundAdapter: Send + Sync {
    /// The wire protocol this adapter speaks.
    fn protocol(&self) -> ApiProtocol;

    /// Render a canonical [`Prompt`] into this protocol's upstream request body.
    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value>;

    /// Parse a provider response body into a canonical [`GenerateResult`].
    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult>;

    /// A fresh stateful decoder turning this protocol's SSE events into
    /// canonical [`StreamPart`]s.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// Whether this protocol can honour
    /// [`Prompt::response_format`](crate::language_model::types::Prompt::response_format).
    /// Default is `false` so out-of-tree custom adapters surface a clear 400
    /// rather than silently dropping the schema. The four built-in adapters
    /// override this to `true`.
    fn supports_response_format(&self) -> bool {
        false
    }
}

/// Dispatch policy for one outbound provider — endpoint URL shape and
/// authentication scheme.
///
/// The built-in protocols ship with their own straightforward transports
/// (Bearer / `x-api-key` / `x-goog-api-key`); a platform-specific provider
/// overrides this trait to install e.g. AWS SigV4 signing or Azure AAD
/// tokens. The trait is async so signers that need credential resolution or
/// async key fetches can run them here.
#[async_trait]
pub trait Transport: Send + Sync {
    /// The wire protocol this transport speaks. Must match the paired
    /// [`OutboundAdapter`]'s [`protocol()`](OutboundAdapter::protocol).
    fn protocol(&self) -> ApiProtocol;

    /// The endpoint URL for an upstream request. `stream` distinguishes the
    /// streaming endpoint from the non-streaming one when the protocol does
    /// (Google encodes it in the path verb).
    fn endpoint_url(&self, target: &RoutingTarget, stream: bool) -> String;

    /// Apply authentication to a fully-built request. The body is already
    /// serialized so signers that hash the body (SigV4, HMAC) can read it.
    /// Receives ownership of the request and returns it ready to send.
    async fn authorise(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request>;
}

/// Stateful decoder: upstream SSE events → canonical stream parts. Streaming
/// protocols are explicit state machines.
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

/// Look up the [`InboundAdapter`] for a built-in protocol. Custom protocols
/// have no inbound adapter — the SDK never serves them to clients.
pub fn inbound_adapter_for(protocol: &ApiProtocol) -> Option<Box<dyn InboundAdapter>> {
    match protocol {
        ApiProtocol::Openai => Some(Box::new(openai_chat::OpenAiChatAdapter)),
        ApiProtocol::Anthropic => Some(Box::new(anthropic::AnthropicAdapter)),
        ApiProtocol::Responses => Some(Box::new(openai_responses::OpenAiResponsesAdapter)),
        ApiProtocol::Google => Some(Box::new(google::GoogleAdapter)),
        ApiProtocol::Custom(_) => None,
    }
}

/// One `(adapter, transport)` pair for one outbound protocol.
struct DispatchEntry {
    adapter: Arc<dyn OutboundAdapter>,
    transport: Arc<dyn Transport>,
}

/// Borrowed `(adapter, transport)` pair returned by [`OutboundDispatch::lookup`].
pub type DispatchHandle<'a> = (&'a Arc<dyn OutboundAdapter>, &'a Arc<dyn Transport>);

/// Registry the executor consults to dispatch an outbound request: maps
/// [`ApiProtocol`] → ([`OutboundAdapter`], [`Transport`]). Built-in protocols
/// are pre-registered by [`OutboundDispatch::builtin`]; plug-in crates call
/// [`register`](Self::register) to add their own.
///
/// See the [module-level docs](self) for a Bedrock-shaped example.
pub struct OutboundDispatch {
    entries: HashMap<ApiProtocol, DispatchEntry>,
}

impl OutboundDispatch {
    /// An empty registry. Useful only for tests; production callers want
    /// [`builtin`](Self::builtin).
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// A registry pre-populated with the four built-in protocols.
    pub fn builtin() -> Self {
        let mut d = Self::empty();
        d.register(
            Arc::new(openai_chat::OpenAiChatAdapter),
            Arc::new(openai_chat::OpenAiChatTransport),
        );
        d.register(
            Arc::new(anthropic::AnthropicAdapter),
            Arc::new(anthropic::AnthropicTransport),
        );
        d.register(
            Arc::new(openai_responses::OpenAiResponsesAdapter),
            Arc::new(openai_responses::OpenAiResponsesTransport),
        );
        d.register(
            Arc::new(google::GoogleAdapter),
            Arc::new(google::GoogleTransport),
        );
        d
    }

    /// Register one `(adapter, transport)` pair. Both must agree on their
    /// `protocol()`; a mismatch is a programming error and panics. Re-
    /// registering the same protocol overwrites the previous entry.
    pub fn register(&mut self, adapter: Arc<dyn OutboundAdapter>, transport: Arc<dyn Transport>) {
        let protocol = adapter.protocol();
        assert_eq!(
            protocol,
            transport.protocol(),
            "OutboundAdapter and Transport must agree on protocol() — got {} vs {}",
            adapter.protocol(),
            transport.protocol(),
        );
        self.entries
            .insert(protocol, DispatchEntry { adapter, transport });
    }

    /// Look up the dispatch entry for `protocol`. Returns the adapter and the
    /// transport as a borrowed pair so callers don't pay a clone per request.
    pub fn lookup(&self, protocol: &ApiProtocol) -> Option<DispatchHandle<'_>> {
        self.entries
            .get(protocol)
            .map(|e| (&e.adapter, &e.transport))
    }
}

impl Default for OutboundDispatch {
    fn default() -> Self {
        Self::builtin()
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
