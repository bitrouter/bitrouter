//! Messages adapter.
//!
//! Official reference: <https://docs.anthropic.com/en/api/messages>
//! Streaming: <https://docs.anthropic.com/en/docs/build-with-claude/streaming>
//!
//! Notable v0 regressions guarded here:
//! - #227 → #228: `system` accepts a string **or** an array of content blocks.
//! - #364: `tool_result.content` accepts a string **or** an array; `thinking`
//!   blocks round-trip.
//! - #416: mixed text + tool_use blocks keep their order and never 502.
//! - #422: inbound `ping` events are ignored, not treated as errors.

use schemars::JsonSchema;
use serde::Deserialize;

use async_trait::async_trait;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, SseEvent, StreamDecoder, StreamEncoder, Transport,
    describe_deser_error,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message, Prompt,
    ResponseFormat, Role, RoutingTarget, StreamPart, Usage,
};

/// The Messages inbound + outbound protocol adapter.
pub struct MessagesAdapter;

/// HTTP transport for Messages: `POST {api_base}/messages` with
/// `x-api-key` + `anthropic-version: 2023-06-01`. The version constant is the
/// only released revision as of 2026-05; cf.
/// <https://platform.claude.com/docs/en/api/versioning>.
pub struct MessagesTransport;

// ===== wire request types =====

/// Messages request body (<https://docs.anthropic.com/en/api/messages>).
///
/// `pub` so downstream crates (notably `bitrouter-cloud`) can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesRequest {
    model: String,
    /// String or an array of `{type:"text", text}` blocks (#227 → #228).
    #[serde(default)]
    system: Option<serde_json::Value>,
    messages: Vec<MessagesMessage>,
    #[serde(default)]
    tools: Vec<MessagesTool>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `stop_sequences`, `top_k`, `metadata`,
    /// `thinking`, … — rides along via `extra` and is splatted back on render.
    /// Skipped from the published schema so the documented contract is the set
    /// of typed fields; pass-through behavior is preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// One element of [`MessagesRequest`]'s `messages` array — a `{ role, content }` turn.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesMessage {
    role: String,
    /// String or an array of content blocks.
    content: serde_json::Value,
}

/// One element of [`MessagesRequest`]'s `tools` array — Anthropic's tool definition shape.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: serde_json::Value,
}

/// Collapse Anthropic's `system` (string or content-block array) into plain text.
fn parse_system(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Messages `tool_result.content` may be a string or an array of blocks (#364).
fn tool_result_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Parse one Anthropic message's content (string or block array) into ordered
/// canonical [`Content`]. Block order is preserved (#416).
fn parse_content(value: &serde_json::Value) -> Result<Vec<Content>> {
    match value {
        serde_json::Value::String(s) => Ok(vec![Content::Text { text: s.clone() }]),
        serde_json::Value::Array(blocks) => {
            let mut out = Vec::with_capacity(blocks.len());
            for block in blocks {
                let block_type = block.get("type").and_then(|t| t.as_str()).ok_or_else(|| {
                    BitrouterError::bad_request("anthropic content block missing 'type'")
                })?;
                match block_type {
                    "text" => out.push(Content::Text {
                        text: block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    // both `thinking` and `redacted_thinking` map to Reasoning
                    "thinking" | "redacted_thinking" => out.push(Content::Reasoning {
                        text: block
                            .get("thinking")
                            .or_else(|| block.get("data"))
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    "tool_use" => out.push(Content::ToolCall {
                        id: block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: block
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                    }),
                    "tool_result" => out.push(Content::ToolResult {
                        call_id: block
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        content: block
                            .get("content")
                            .map(tool_result_text)
                            .unwrap_or_default(),
                    }),
                    // Unknown block types are skipped, not fatal — forward
                    // compatibility (an explicit decision, not a catch-all bug).
                    other => {
                        tracing::debug!(block_type = other, "skipping unknown anthropic block");
                    }
                }
            }
            Ok(out)
        }
        _ => Err(BitrouterError::bad_request(
            "anthropic message 'content' must be a string or an array",
        )),
    }
}

fn parse_role(role: &str) -> Result<Role> {
    match role {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        // Messages has no top-level system/tool role; tool results ride inside
        // a user-role message. An unexpected role is a hard error (#454-4).
        other => Err(BitrouterError::bad_request(format!(
            "unknown anthropic message role '{other}' (expected user/assistant)"
        ))),
    }
}

fn stop_reason_to_finish(s: &str) -> Option<FinishReason> {
    match s {
        "end_turn" | "stop_sequence" => Some(FinishReason::Stop),
        "max_tokens" => Some(FinishReason::Length),
        "tool_use" => Some(FinishReason::ToolCalls),
        "refusal" => Some(FinishReason::ContentFilter),
        // Unknown but provider-supplied — keep verbatim so observability can
        // see it. Anthropic also documents `pause_turn`.
        other => Some(FinishReason::Other(other.to_string())),
    }
}

/// Map Anthropic's `error.type` to an HTTP status code so a mid-stream 4xx
/// upstream error doesn't blanket-convert to 502 (which masks
/// `invalid_request_error` / `rate_limit_error` / `authentication_error`).
/// Ref: <https://docs.anthropic.com/en/api/errors>.
fn messages_error_status(err_type: &str) -> u16 {
    match err_type {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        "api_error" | "" => 502,
        _ => 502,
    }
}

fn finish_to_stop_reason(r: &FinishReason) -> String {
    match r {
        FinishReason::Stop => "end_turn".to_string(),
        FinishReason::Length => "max_tokens".to_string(),
        FinishReason::ToolCalls => "tool_use".to_string(),
        FinishReason::ContentFilter => "refusal".to_string(),
        FinishReason::Other(s) => s.clone(),
        // Messages has no native "error" finish — pick `end_turn` so the
        // wire envelope is well-formed; outbound encoders emit a separate
        // error frame ahead of this when the canonical IR carries an error.
        FinishReason::Error(_) => "end_turn".to_string(),
    }
}

impl InboundAdapter for MessagesAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Messages
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        let req: MessagesRequest = serde_json::from_value(body.clone())
            .map_err(|e| describe_deser_error("MessagesRequest", &e, &body))?;

        let system = req
            .system
            .as_ref()
            .map(parse_system)
            .filter(|s| !s.is_empty());

        let mut messages = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            let role = parse_role(&m.role)?;
            // A user-role message may carry tool_result blocks — split those
            // into a canonical Tool-role message so the IR stays clean.
            let parsed = parse_content(&m.content)?;
            let (tool_results, rest): (Vec<_>, Vec<_>) = parsed
                .into_iter()
                .partition(|c| matches!(c, Content::ToolResult { .. }));
            if !tool_results.is_empty() {
                messages.push(Message {
                    role: Role::Tool,
                    content: tool_results,
                });
            }
            if !rest.is_empty() {
                messages.push(Message {
                    role,
                    content: rest,
                });
            }
        }

        let tools = req
            .tools
            .into_iter()
            .map(|t| crate::language_model::types::Tool {
                name: t.name,
                description: t.description,
                parameters: t.input_schema,
            })
            .collect();

        // Messages' GA structured-outputs lives under
        // `output_config.format`. Some clients still emit the deprecated flat
        // `output_format` (vercel/ai#12298) — accept it as a graceful alias.
        // Only the alias that actually matched is stripped from extras, so
        // unknown siblings inside `output_config` survive opaquely if
        // `output_format` was the source.
        let mut extra = req.extra;
        let response_format = if let Some(rf) = parse_output_config_format(&extra) {
            if let Some(oc) = extra
                .get_mut("output_config")
                .and_then(|v| v.as_object_mut())
            {
                oc.remove("format");
                if oc.is_empty() {
                    extra.remove("output_config");
                }
            }
            Some(rf)
        } else if let Some(rf) = parse_legacy_output_format(&extra) {
            extra.remove("output_format");
            Some(rf)
        } else {
            None
        };

        Ok(Prompt {
            model: req.model,
            system,
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_tokens,
                reasoning_effort: None,
                extra,
            },
            response_format,
            stream: req.stream,
        })
    }

    fn render_response(
        &self,
        result: &GenerateResult,
        prompt: &Prompt,
        request_id: &str,
    ) -> Result<serde_json::Value> {
        // Content blocks keep their canonical order (#416).
        let content: Vec<serde_json::Value> = result
            .content
            .iter()
            .filter_map(render_content_block)
            .collect();
        let usage = result.usage.unwrap_or_default();
        Ok(serde_json::json!({
            "id": request_id,
            "type": "message",
            "role": "assistant",
            "model": prompt.model,
            "content": content,
            "stop_reason": result.finish_reason.as_ref().map(finish_to_stop_reason),
            "usage": render_usage(&usage),
        }))
    }

    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder> {
        Box::new(MessagesStreamEncoder {
            request_id: request_id.to_string(),
            model: model.to_string(),
            started: false,
            block_open: false,
            block_kind: None,
            block_index: 0,
        })
    }
}

impl OutboundAdapter for MessagesAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Messages
    }

    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value> {
        let mut messages = Vec::new();
        for m in &prompt.messages {
            messages.push(render_message(m));
        }
        let mut req = serde_json::Map::new();
        req.insert("model".into(), prompt.model.clone().into());
        if let Some(system) = &prompt.system {
            req.insert("system".into(), system.clone().into());
        }
        req.insert("messages".into(), messages.into());
        if !prompt.tools.is_empty() {
            req.insert(
                "tools".into(),
                prompt
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        // Messages requires max_tokens; default to a sane ceiling if unset.
        req.insert(
            "max_tokens".into(),
            prompt.params.max_tokens.unwrap_or(4096).into(),
        );
        if let Some(t) = prompt.params.temperature {
            req.insert("temperature".into(), t.into());
        }
        if let Some(p) = prompt.params.top_p {
            req.insert("top_p".into(), p.into());
        }
        // Render the canonical response_format into Anthropic's GA shape
        // (`output_config.format`). `name` and `strict` are intentionally
        // dropped — Anthropic's schema-constrained sampling has no concept of
        // either and they would be rejected as unknown fields.
        if let Some(rf) = &prompt.response_format {
            req.insert(
                "output_config".into(),
                serde_json::json!({ "format": render_messages_response_format(rf) }),
            );
        }
        // Splat anthropic-specific extras (tool_choice, stop_sequences, …) back
        // into the outbound request. Typed fields win over same-named extras.
        for (k, v) in &prompt.params.extra {
            req.entry(k.clone()).or_insert_with(|| v.clone());
        }
        req.insert("stream".into(), prompt.stream.into());
        Ok(serde_json::Value::Object(req))
    }

    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult> {
        let content_blocks = body
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| BitrouterError::bad_request("anthropic response missing 'content'"))?;
        let mut content = Vec::with_capacity(content_blocks.len());
        for block in content_blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => content.push(Content::Text {
                    text: block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }),
                Some("thinking") | Some("redacted_thinking") => content.push(Content::Reasoning {
                    text: block
                        .get("thinking")
                        .or_else(|| block.get("data"))
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }),
                Some("tool_use") => content.push(Content::ToolCall {
                    id: block
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: block
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".to_string()),
                }),
                _ => {}
            }
        }
        let finish_reason = body
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .and_then(stop_reason_to_finish);
        let usage = body.get("usage").and_then(parse_usage);
        // Messages: top-level `id` (`msg_...`).
        // <https://docs.anthropic.com/en/api/messages>
        let response_id = body
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
            response_id,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(MessagesStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

/// Lift Anthropic's GA `output_config.format: { type: "json_schema", schema }`
/// into the canonical [`ResponseFormat`]. Anthropic's wire format carries no
/// `name` / `strict` so both are `None`.
fn parse_output_config_format(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<ResponseFormat> {
    json_schema_from(extra.get("output_config")?.get("format")?)
}

/// Lift Anthropic's deprecated flat `output_format: { type: "json_schema", schema }`
/// into the canonical [`ResponseFormat`]. Kept as a graceful alias for
/// clients still emitting the pre-GA shape (vercel/ai#12298).
fn parse_legacy_output_format(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<ResponseFormat> {
    json_schema_from(extra.get("output_format")?)
}

fn json_schema_from(format: &serde_json::Value) -> Option<ResponseFormat> {
    if format.get("type")?.as_str()? != "json_schema" {
        return None;
    }
    Some(ResponseFormat::JsonSchema {
        name: None,
        strict: None,
        schema: format.get("schema")?.clone(),
    })
}

/// Render a canonical [`ResponseFormat`] into Anthropic's
/// `{ type: "json_schema", schema }` body that sits under `output_config.format`.
fn render_messages_response_format(rf: &ResponseFormat) -> serde_json::Value {
    let ResponseFormat::JsonSchema { schema, .. } = rf;
    serde_json::json!({ "type": "json_schema", "schema": schema })
}

#[async_trait]
impl Transport for MessagesTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Messages
    }

    fn endpoint_url(&self, target: &RoutingTarget, _stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        format!("{base}/messages")
    }

    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let key = target.effective_api_key();
        let key_header = reqwest::header::HeaderValue::from_str(key).map_err(|e| {
            BitrouterError::internal(format!("invalid api key for x-api-key header: {e}"))
        })?;
        request.headers_mut().insert("x-api-key", key_header);
        request.headers_mut().insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        Ok(request)
    }
}

fn render_content_block(c: &Content) -> Option<serde_json::Value> {
    match c {
        Content::Text { text } => Some(serde_json::json!({ "type": "text", "text": text })),
        Content::Reasoning { text } => {
            Some(serde_json::json!({ "type": "thinking", "thinking": text }))
        }
        Content::ToolCall {
            id,
            name,
            arguments,
        } => {
            let input: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
            Some(serde_json::json!({
                "type": "tool_use", "id": id, "name": name, "input": input,
            }))
        }
        // tool results are request-side only; not part of an assistant reply
        Content::ToolResult { .. } => None,
    }
}

fn render_message(m: &Message) -> serde_json::Value {
    // Canonical Tool-role messages become Anthropic user messages carrying
    // tool_result blocks.
    if m.role == Role::Tool {
        let blocks: Vec<serde_json::Value> = m
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolResult { call_id, content } => Some(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                })),
                _ => None,
            })
            .collect();
        return serde_json::json!({ "role": "user", "content": blocks });
    }

    let role = match m.role {
        Role::Assistant => "assistant",
        // System should have been lifted to the top-level `system` field; if a
        // System message slips through, fold it into a user turn.
        Role::User | Role::System => "user",
        Role::Tool => unreachable!("handled above"),
    };
    let blocks: Vec<serde_json::Value> =
        m.content.iter().filter_map(render_content_block).collect();
    serde_json::json!({ "role": role, "content": blocks })
}

fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    if value.get("input_tokens").is_none() && value.get("output_tokens").is_none() {
        return None;
    }
    let input = value
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Cache fields are Anthropic-specific; absent on providers that don't
    // implement prompt caching. Refs:
    // - <https://docs.anthropic.com/en/api/messages> → `usage` object
    // - <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
    //   → "Tracking cache performance"
    //
    // Wire vs SDK contract: Anthropic's `input_tokens` is the *uncached*
    // portion of the prompt (tokens after the last cache breakpoint) and
    // is reported **alongside** (not inclusive of) `cache_read_input_tokens`
    // / `cache_creation_input_tokens`. The prompt-caching guide documents
    // the relationship explicitly:
    //
    //     total_input_tokens
    //         = cache_read_input_tokens
    //         + cache_creation_input_tokens
    //         + input_tokens
    //
    // The canonical [`Usage::cache_read_tokens`] / [`Usage::cache_write_tokens`]
    // are documented as subsets of [`Usage::prompt_tokens`] — matching how
    // Chat Completions / Responses and Generate Content report cached prompt tokens.
    //
    // Without folding the cache buckets back into `prompt_tokens` here,
    // downstream billing layers that derive `no_cache = prompt_tokens -
    // cache_read - cache_write` would saturate to 0 on a cache-heavy
    // request and silently undercharge the uncached portion. Fold them
    // (saturating against `u64::MAX` so a malicious upstream can't
    // overflow the sum) so the canonical IR matches its own contract.
    let cache_read = value
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_write = value
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: input.saturating_add(cache_read).saturating_add(cache_write),
        completion_tokens: output,
        reasoning_tokens: 0,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    })
}

// ===== streaming =====

/// Messages SSE decoder. Explicit state machine over the
/// `message_start` / `content_block_start` / `content_block_delta` /
/// `content_block_stop` / `message_delta` / `message_stop` / `ping` / `error`
/// event set.
#[derive(Default)]
struct MessagesStreamDecoder {
    /// per content-block index → tool id (empty string for non-tool blocks),
    /// so an `input_json_delta` knows which canonical tool call it belongs to.
    block_tool_ids: Vec<String>,
    usage: Usage,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

impl StreamDecoder for MessagesStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let event_name = event.event.as_deref().unwrap_or_default();
        // ping / unknown events are intentionally ignored — never errors (#422).
        if event_name == "ping" || event.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let json: serde_json::Value = match serde_json::from_str(event.data.trim()) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };

        let mut parts = Vec::new();
        match event_name {
            "message_start" => {
                // `message_start` fires exactly once and carries the message
                // id (`msg_...`); surface it for observability. Spec:
                // <https://docs.anthropic.com/en/api/messages-streaming>
                if let Some(id) = json
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                {
                    parts.push(StreamPart::ResponseStarted { id: id.to_string() });
                }
                // Messages emits the prompt-cache stats on the start frame,
                // so capture them now and propagate via the terminal Usage
                // part.
                if let Some(usage) = json.get("message").and_then(|m| m.get("usage"))
                    && let Some(parsed) = parse_usage(usage)
                {
                    self.usage = parsed;
                }
            }
            "content_block_start" => {
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let block = json.get("content_block");
                let kind = match block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) {
                    Some("thinking") | Some("redacted_thinking") => BlockKind::Thinking,
                    Some("tool_use") => BlockKind::ToolUse,
                    _ => BlockKind::Text,
                };
                let tool_id = block
                    .and_then(|b| b.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                while self.block_tool_ids.len() <= index {
                    self.block_tool_ids.push(String::new());
                }
                self.block_tool_ids[index] = tool_id.clone();
                if kind == BlockKind::ToolUse {
                    // emit an opening ToolCallDelta carrying the tool name
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string());
                    parts.push(StreamPart::ToolCallDelta {
                        id: tool_id,
                        name,
                        arguments: String::new(),
                    });
                }
            }
            "content_block_delta" => {
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let delta = json.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) =
                            delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                        {
                            parts.push(StreamPart::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(|t| t.as_str())
                        {
                            parts.push(StreamPart::ReasoningDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        let id = self.block_tool_ids.get(index).cloned().unwrap_or_default();
                        if let Some(partial) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|t| t.as_str())
                        {
                            parts.push(StreamPart::ToolCallDelta {
                                id,
                                name: None,
                                arguments: partial.to_string(),
                            });
                        }
                    }
                    // any other delta type: ignore, do not error
                    _ => {}
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                // `message_delta.usage` carries the cumulative final counts
                // (<https://docs.anthropic.com/en/api/messages-streaming>).
                // Messages emits its wire-level `input_tokens` (the
                // *uncached* portion, per
                // <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>)
                // alongside the cache buckets; the canonical
                // [`Usage::prompt_tokens`] is inclusive of those buckets
                // (matches `parse_usage` above). Back out the prior exclusive
                // input from the inclusive total so a delta that only
                // refreshes a subset of fields still recomputes
                // `prompt_tokens` consistently.
                if let Some(u) = json.get("usage") {
                    let prior_excl_input = self
                        .usage
                        .prompt_tokens
                        .saturating_sub(self.usage.cache_read_tokens)
                        .saturating_sub(self.usage.cache_write_tokens);
                    let new_excl_input = u
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(prior_excl_input);
                    if let Some(cache_read) =
                        u.get("cache_read_input_tokens").and_then(|v| v.as_u64())
                    {
                        self.usage.cache_read_tokens = cache_read;
                    }
                    if let Some(cache_write) = u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        self.usage.cache_write_tokens = cache_write;
                    }
                    if let Some(output) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                        self.usage.completion_tokens = output;
                    }
                    self.usage.prompt_tokens = new_excl_input
                        .saturating_add(self.usage.cache_read_tokens)
                        .saturating_add(self.usage.cache_write_tokens);
                }
                if let Some(reason) = json
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                    .and_then(stop_reason_to_finish)
                {
                    parts.push(StreamPart::Usage { usage: self.usage });
                    parts.push(StreamPart::Finish { reason });
                }
            }
            "message_stop" => {}
            "error" => {
                // Mid-stream error — derive the HTTP status from Anthropic's
                // `error.type` so 4xx upstream errors pass through to the
                // caller (instead of always 502'ing and triggering fallback
                // retries). Spec: <https://docs.anthropic.com/en/api/errors>.
                let error_obj = json.get("error");
                let err_type = error_obj
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let msg = error_obj
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("anthropic stream error");
                return Err(BitrouterError::Upstream {
                    status: messages_error_status(err_type),
                    message: msg.to_string(),
                });
            }
            // genuinely unknown event name — forward-compatible: ignore
            _ => {}
        }
        Ok(parts)
    }
}

/// Messages SSE encoder. Emits the full event envelope: `message_start`,
/// per-block `content_block_start` / `_delta` / `_stop`, `message_delta`,
/// `message_stop`.
///
/// Block transitions: Anthropic's `content_block_*` events are typed
/// (`text` / `thinking` / `tool_use`). Strict clients (e.g. Claude Code)
/// reject a `text_delta` inside a still-open `thinking` block. The encoder
/// therefore tracks the **kind** of the currently open block and closes it
/// before opening a new block of a different kind (text → thinking, etc.).
/// Same-kind consecutive deltas reuse the open block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderBlockKind {
    Text,
    Thinking,
    ToolUse,
}

struct MessagesStreamEncoder {
    request_id: String,
    model: String,
    started: bool,
    block_open: bool,
    /// The kind of the currently open block, used to detect transitions.
    /// Meaningful only when `block_open == true`.
    block_kind: Option<EncoderBlockKind>,
    block_index: usize,
}

impl MessagesStreamEncoder {
    fn ev(name: &str, data: serde_json::Value) -> SseFrame {
        SseFrame::Event {
            event: Some(name.to_string()),
            data: data.to_string(),
        }
    }

    fn ensure_started(&mut self, frames: &mut Vec<SseFrame>) {
        if !self.started {
            self.started = true;
            frames.push(Self::ev(
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": self.request_id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    }
                }),
            ));
        }
    }

    fn close_block(&mut self, frames: &mut Vec<SseFrame>) {
        if self.block_open {
            frames.push(Self::ev(
                "content_block_stop",
                serde_json::json!({ "type": "content_block_stop", "index": self.block_index }),
            ));
            self.block_open = false;
            self.block_kind = None;
            self.block_index += 1;
        }
    }

    /// If a block of a different kind is currently open, close it; then ensure
    /// a block of `wanted` is open. Returns whether a *new* block was opened
    /// this call so the caller can append the right `content_block_start`.
    fn ensure_block_open(&mut self, frames: &mut Vec<SseFrame>, wanted: EncoderBlockKind) -> bool {
        if self.block_open && self.block_kind != Some(wanted) {
            self.close_block(frames);
        }
        if !self.block_open {
            self.block_kind = Some(wanted);
            self.block_open = true;
            true
        } else {
            false
        }
    }

    /// Emit the terminal `message_delta` + `message_stop` frames. Shared by the
    /// `Finish` and `ResponseCompleted` encode arms. Anthropic's
    /// `message_delta.usage` carries `output_tokens`, `input_tokens`, and the
    /// two cache fields when present
    /// (<https://docs.anthropic.com/en/api/messages-streaming>).
    fn emit_terminal(
        &mut self,
        frames: &mut Vec<SseFrame>,
        stop_reason: &str,
        usage: Option<Usage>,
    ) {
        self.close_block(frames);
        let u = usage.unwrap_or_default();
        frames.push(Self::ev(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason },
                "usage": render_usage(&u),
            }),
        ));
        frames.push(Self::ev(
            "message_stop",
            serde_json::json!({ "type": "message_stop" }),
        ));
    }
}

/// Build an Anthropic-shaped `usage` object that always emits
/// `input_tokens` / `output_tokens` and adds the cache fields when non-zero.
///
/// Wire format note: Anthropic reports `input_tokens` as the *uncached*
/// portion of the prompt — the cache buckets are reported alongside, not
/// included in `input_tokens`. The prompt-caching guide documents the
/// relationship as `total = cache_read + cache_creation + input_tokens`
/// (<https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
/// → "Tracking cache performance"). The canonical [`Usage::prompt_tokens`]
/// is the **inclusive** total (matches Chat Completions / Generate Content semantics; see
/// `parse_usage`), so we subtract the cache buckets back out here to
/// reconstruct the wire format. Saturating-sub guards against a caller
/// constructing a `Usage` whose cache totals exceed `prompt_tokens` —
/// shouldn't happen in practice, but a malformed canonical IR shouldn't
/// underflow the wire payload either.
fn render_usage(u: &Usage) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let excl_input = u
        .prompt_tokens
        .saturating_sub(u.cache_read_tokens)
        .saturating_sub(u.cache_write_tokens);
    map.insert("input_tokens".into(), excl_input.into());
    map.insert("output_tokens".into(), u.completion_tokens.into());
    if u.cache_read_tokens > 0 {
        map.insert("cache_read_input_tokens".into(), u.cache_read_tokens.into());
    }
    if u.cache_write_tokens > 0 {
        map.insert(
            "cache_creation_input_tokens".into(),
            u.cache_write_tokens.into(),
        );
    }
    serde_json::Value::Object(map)
}

impl StreamEncoder for MessagesStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        self.ensure_started(&mut frames);
        match part {
            StreamPart::TextDelta { text } => {
                if self.ensure_block_open(&mut frames, EncoderBlockKind::Text) {
                    frames.push(Self::ev(
                        "content_block_start",
                        serde_json::json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": { "type": "text", "text": "" },
                        }),
                    ));
                }
                frames.push(Self::ev(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": { "type": "text_delta", "text": text },
                    }),
                ));
            }
            StreamPart::ReasoningDelta { text } => {
                if self.ensure_block_open(&mut frames, EncoderBlockKind::Thinking) {
                    frames.push(Self::ev(
                        "content_block_start",
                        serde_json::json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": { "type": "thinking", "thinking": "" },
                        }),
                    ));
                }
                frames.push(Self::ev(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": { "type": "thinking_delta", "thinking": text },
                    }),
                ));
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                if let Some(name) = name {
                    // A new tool call always opens its own block, even if a
                    // tool_use block was already open (consecutive tool calls
                    // are distinct blocks). Force-close, then open.
                    self.close_block(&mut frames);
                    self.ensure_block_open(&mut frames, EncoderBlockKind::ToolUse);
                    frames.push(Self::ev(
                        "content_block_start",
                        serde_json::json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
                        }),
                    ));
                }
                if !arguments.is_empty() {
                    frames.push(Self::ev(
                        "content_block_delta",
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": { "type": "input_json_delta", "partial_json": arguments },
                        }),
                    ));
                }
            }
            StreamPart::Usage { .. } => {}
            StreamPart::ResponseStarted { .. } => {
                // Observability-only metadata (upstream response id); the
                // Messages-protocol client gets its id from the
                // `message_start` event `ensure_started` emits.
            }
            StreamPart::Finish { reason } => {
                self.emit_terminal(&mut frames, &finish_to_stop_reason(reason), None);
            }
            StreamPart::ResponseCompleted { status, usage, .. } => {
                // Inbound was Responses; map its status onto Messages'
                // `stop_reason` and carry the usage if present.
                let stop_reason = if status == "incomplete" {
                    "max_tokens"
                } else {
                    "end_turn"
                };
                self.emit_terminal(&mut frames, stop_reason, *usage);
            }
        }
        Ok(frames)
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Messages surfaces a mid-stream error as a named `error` event.
        vec![Self::ev(
            "error",
            serde_json::json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            }),
        )]
    }
}
