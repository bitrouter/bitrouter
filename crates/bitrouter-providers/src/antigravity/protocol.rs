//! The `Custom("antigravity")` protocol — Gemini `generateContent` retargeted at
//! Google's Code Assist backend (`cloudcode-pa.googleapis.com/v1internal:*`).
//!
//! cloudcode-pa speaks the same Gemini content shape as the public Generative
//! Language API, with two wire differences:
//!
//! 1. **Method endpoint**: `POST {base}/v1internal:{verb}` (the model+project
//!    ride in the request body envelope), not `.../models/{model}:{verb}`.
//! 2. **Response envelope**: replies (and every streamed chunk) are wrapped in
//!    `{"response": {…gemini…}, "traceId": …, "metadata": …}`.
//!
//! So this [`OutboundAdapter`]/[`Transport`] pair **composes** the built-in
//! [`GenerateContentAdapter`]: render + parse delegate to it, we only override
//! the endpoint URL and unwrap the `response` envelope before the Gemini parser
//! sees it. The request-body envelope (`{model, project, request}`) and auth are
//! added by [`super::AntigravityAuthApplier`], which registers under the same
//! provider id.
//!
//! Registered on the [`OutboundDispatch`](bitrouter_sdk::language_model::OutboundDispatch) at app startup (see
//! `apps/bitrouter`), keyed by the `Custom("antigravity")` protocol the registry
//! selects via `api_protocol: antigravity`.

use async_trait::async_trait;
use serde_json::Value;

use bitrouter_sdk::language_model::protocol::generate_content::{
    GenerateContentAdapter, GenerateContentTransport,
};
use bitrouter_sdk::language_model::{
    ApiProtocol, GenerateResult, OutboundAdapter, Prompt, RoutingTarget, SseEvent, StreamDecoder,
    StreamPart, Transport,
};
use bitrouter_sdk::{Result, language_model::protocol};

/// The `Custom(_)` protocol name. The registry selects this via
/// `api_protocol: antigravity`; the app registers the adapter/transport under it.
pub const PROTOCOL: &str = "antigravity";

/// The JSON key cloudcode-pa wraps every response (and stream chunk) in.
const RESPONSE_ENVELOPE_KEY: &str = "response";

fn antigravity_protocol() -> ApiProtocol {
    ApiProtocol::Custom(PROTOCOL.to_string())
}

/// Unwrap the cloudcode-pa `{"response": …}` envelope, returning the inner
/// Gemini value. Passes a value through unchanged when the key is absent (so a
/// bare response, or a future shape, still parses).
fn unwrap_response(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut()
        && let Some(inner) = obj.remove(RESPONSE_ENVELOPE_KEY)
    {
        return inner;
    }
    body
}

/// Outbound adapter: delegates Gemini render/parse to [`GenerateContentAdapter`],
/// overriding only the response-envelope unwrap.
pub struct AntigravityAdapter {
    inner: GenerateContentAdapter,
}

impl AntigravityAdapter {
    /// Construct the adapter.
    pub fn new() -> Self {
        Self {
            inner: GenerateContentAdapter,
        }
    }
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundAdapter for AntigravityAdapter {
    fn protocol(&self) -> ApiProtocol {
        antigravity_protocol()
    }

    fn render_request(&self, prompt: &Prompt) -> Result<Value> {
        // The bare Gemini body; the applier wraps it in the
        // `{model, project, request}` envelope in `prepare_body`.
        self.inner.render_request(prompt)
    }

    fn parse_response(&self, body: Value) -> Result<GenerateResult> {
        self.inner.parse_response(unwrap_response(body))
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AntigravityStreamDecoder {
            inner: self.inner.stream_decoder(),
        })
    }

    fn supports_response_format(&self) -> bool {
        self.inner.supports_response_format()
    }
}

/// Transport: `POST {base}/v1internal:{verb}` (method endpoint), Bearer auth
/// handled by the applier.
pub struct AntigravityTransport;

#[async_trait]
impl Transport for AntigravityTransport {
    fn protocol(&self) -> ApiProtocol {
        antigravity_protocol()
    }

    fn endpoint_url(&self, target: &RoutingTarget, stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        let verb = if stream {
            "v1internal:streamGenerateContent?alt=sse"
        } else {
            "v1internal:generateContent"
        };
        format!("{base}/{verb}")
    }

    async fn authorise(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        // Auth is owned by `AntigravityAuthApplier`; the executor calls the
        // applier's `apply` instead of this when an applier is registered for
        // the provider. This path only runs if the applier is somehow absent —
        // fall back to the Gemini transport's default rather than sending
        // unauthenticated.
        GenerateContentTransport.authorise(request, target).await
    }
}

/// Stream decoder wrapping the Gemini decoder: unwraps `{"response": …}` from
/// each SSE chunk before the Gemini state machine parses it.
struct AntigravityStreamDecoder {
    inner: Box<dyn StreamDecoder>,
}

impl StreamDecoder for AntigravityStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let data = event.data.trim();
        // Only rewrite well-formed JSON chunks carrying the envelope; anything
        // else (blank keepalives, `[DONE]`, already-bare chunks) passes through
        // to the inner decoder untouched.
        let rewritten = match serde_json::from_str::<Value>(data) {
            Ok(v) if v.get(RESPONSE_ENVELOPE_KEY).is_some() => Some(SseEvent {
                event: event.event.clone(),
                data: unwrap_response(v).to_string(),
            }),
            _ => None,
        };
        match rewritten {
            Some(ref ev) => self.inner.decode(ev),
            None => self.inner.decode(event),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>> {
        self.inner.finish()
    }
}

/// Register the Antigravity adapter + transport on an [`OutboundDispatch`](bitrouter_sdk::language_model::OutboundDispatch) under
/// the `Custom("antigravity")` protocol. Called at app startup.
pub fn register(dispatch: &mut protocol::OutboundDispatch) {
    dispatch.register(
        std::sync::Arc::new(AntigravityAdapter::new()),
        std::sync::Arc::new(AntigravityTransport),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::types::ApiProtocol;

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "antigravity".into(),
            service_id: "gemini-2.5-flash".into(),
            api_base: "https://cloudcode-pa.googleapis.com".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Custom(PROTOCOL.into()),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
        }
    }

    #[test]
    fn endpoint_is_v1internal_method_style() {
        let t = AntigravityTransport;
        assert_eq!(
            t.endpoint_url(&target(), false),
            "https://cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
        assert_eq!(
            t.endpoint_url(&target(), true),
            "https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn unwrap_strips_response_envelope() {
        let wrapped = serde_json::json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "hi"}]}}]},
            "traceId": "t", "metadata": {}
        });
        let inner = unwrap_response(wrapped);
        assert!(inner.get("candidates").is_some());
        assert!(inner.get("traceId").is_none());
    }

    #[test]
    fn unwrap_passes_bare_response_through() {
        let bare = serde_json::json!({"candidates": []});
        assert_eq!(unwrap_response(bare.clone()), bare);
    }

    #[test]
    fn parse_response_unwraps_then_delegates_to_gemini() {
        let adapter = AntigravityAdapter::new();
        let wrapped = serde_json::json!({
            "response": {
                "candidates": [{
                    "content": {"parts": [{"text": "agy-envelope-ok"}], "role": "model"},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2}
            },
            "traceId": "abc"
        });
        let result = adapter.parse_response(wrapped).expect("parse");
        let text = result
            .content
            .iter()
            .find_map(|c| match c {
                bitrouter_sdk::language_model::Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("text content");
        assert_eq!(text, "agy-envelope-ok");
    }

    #[test]
    fn stream_decoder_unwraps_chunk_before_gemini_parse() {
        let adapter = AntigravityAdapter::new();
        let mut dec = adapter.stream_decoder();
        let chunk = serde_json::json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "hello"}]}}]},
            "traceId": "t"
        });
        let parts = dec
            .decode(&SseEvent {
                event: None,
                data: chunk.to_string(),
            })
            .expect("decode");
        let got_text = parts
            .iter()
            .any(|p| matches!(p, StreamPart::TextDelta { text } if text == "hello"));
        assert!(got_text, "expected a TextDelta 'hello', got {parts:?}");
    }
}
