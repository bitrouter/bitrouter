//! OpenAI Responses adapter.
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

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

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

/// The OpenAI Responses protocol adapter.
pub struct OpenAiResponsesAdapter;

/// HTTP transport for OpenAI Responses: `POST {api_base}/responses` with
/// `Authorization: Bearer <api_key>`.
pub struct OpenAiResponsesTransport;

// ===== wire request types =====

/// OpenAI Responses request body
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
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `parallel_tool_calls`,
    /// `max_tool_calls`, `metadata`, `include[]`, `previous_response_id`,
    /// `store`, `stream_options`, … — rides through here and is splatted
    /// back on render. Skipped from the published schema so the documented
    /// contract is the set of typed fields; pass-through behavior is
    /// preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// One element of [`ResponsesRequest`]'s `tools` array — the Responses-flavoured
/// flat tool definition (`{ type: "function", name, description, parameters }`).
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

/// Extract text from a Responses `content` value: a string, or an array of
/// `{type:"input_text"|"output_text"|"text", text}` parts.
fn content_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
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
                            let text = item.get("content").map(content_text).unwrap_or_default();
                            messages.push(Message::text(role, text));
                        }
                    }
                    Some("function_call") => {
                        messages.push(Message {
                            role: Role::Assistant,
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
                            }],
                        });
                    }
                    Some("function_call_output") => {
                        messages.push(Message {
                            role: Role::Tool,
                            content: vec![Content::ToolResult {
                                call_id: item
                                    .get("call_id")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                content: item
                                    .get("output")
                                    .map(|o| match o {
                                        serde_json::Value::String(s) => s.clone(),
                                        other => other.to_string(),
                                    })
                                    .unwrap_or_default(),
                            }],
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
                                content: vec![Content::Reasoning { text }],
                            });
                        }
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

fn finish_from_status(status: &str) -> Option<FinishReason> {
    match status {
        "completed" => Some(FinishReason::Stop),
        // #432: `incomplete` is a valid terminal status, not an error.
        "incomplete" => Some(FinishReason::Length),
        _ => None,
    }
}

impl InboundAdapter for OpenAiResponsesAdapter {
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

        let tools = req
            .tools
            .into_iter()
            .filter(|t| t.kind.as_deref() == Some("function") || t.name.is_some())
            .map(|t| crate::language_model::types::Tool {
                name: t.name.unwrap_or_default(),
                description: t.description,
                parameters: if t.parameters.is_null() {
                    serde_json::json!({})
                } else {
                    t.parameters
                },
            })
            .collect();

        // Promote `text.format: { type: "json_schema", ... }` out of extras
        // into the canonical slot. Other contents of `text` (e.g. `verbosity`)
        // stay in extras and pass through opaquely.
        let mut extra = req.extra;
        let response_format = parse_responses_response_format(&extra);
        if response_format.is_some()
            && let Some(text) = extra.get_mut("text").and_then(|t| t.as_object_mut())
        {
            text.remove("format");
            if text.is_empty() {
                extra.remove("text");
            }
        }

        Ok(Prompt {
            model: req.model,
            system,
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_output_tokens,
                reasoning_effort: req.reasoning.and_then(|r| r.effort),
                // Splat every Responses-API field without a typed slot —
                // tool_choice, parallel_tool_calls, max_tool_calls, metadata,
                // include[], previous_response_id, store, stream_options, … —
                // into `extra` so render_request can put them back.
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
        let output = render_output_items(result);
        let mut body = serde_json::json!({
            "id": request_id,
            "object": "response",
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
        if let Some(usage) = result.usage {
            body["usage"] = render_responses_usage(&usage);
        }
        Ok(body)
    }

    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder> {
        Box::new(ResponsesStreamEncoder {
            request_id: request_id.to_string(),
            model: model.to_string(),
            seq: 0,
            created: false,
            next_output_index: 0,
            reasoning_item: None,
            text_item: None,
            tool_item: None,
            completed_items: Vec::new(),
        })
    }
}

impl OutboundAdapter for OpenAiResponsesAdapter {
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
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        })
                    })
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
        // Render the canonical response_format into Responses' `text.format`.
        // Merge with any extras-supplied `text` blob so caller-supplied
        // sibling keys (e.g. `verbosity`) survive.
        if let Some(rf) = &prompt.response_format {
            let mut text = prompt
                .params
                .extra
                .get("text")
                .and_then(|t| t.as_object().cloned())
                .unwrap_or_default();
            text.insert("format".into(), render_responses_response_format(rf));
            req.insert("text".into(), serde_json::Value::Object(text));
        }
        // Splat Responses-API extras (tool_choice, parallel_tool_calls,
        // metadata, include, …) back onto the outbound request. Typed fields
        // win.
        for (k, v) in &prompt.params.extra {
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
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                content.push(Content::Text {
                                    text: text.to_string(),
                                });
                            }
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
                        content.push(Content::Reasoning { text });
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
                    });
                }
                _ => {}
            }
        }
        let finish_reason = body
            .get("status")
            .and_then(|s| s.as_str())
            .and_then(finish_from_status);
        let usage = body.get("usage").and_then(parse_usage);
        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

/// Detect a `text.format: { type: "json_schema", ... }` blob in the inbound
/// extras and lift it into the canonical [`ResponseFormat`]. Returns `None`
/// for any other shape.
fn parse_responses_response_format(
    extra: &HashMap<String, serde_json::Value>,
) -> Option<ResponseFormat> {
    let format = extra.get("text")?.get("format")?;
    if format.get("type")?.as_str()? != "json_schema" {
        return None;
    }
    let schema = format.get("schema")?.clone();
    Some(ResponseFormat::JsonSchema {
        name: format
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string()),
        strict: format.get("strict").and_then(|s| s.as_bool()),
        schema,
    })
}

/// Render a canonical [`ResponseFormat`] into Responses' native
/// `{ type: "json_schema", name, strict, schema }` body that sits under
/// `text.format`. OpenAI requires `name`; default it.
fn render_responses_response_format(rf: &ResponseFormat) -> serde_json::Value {
    let ResponseFormat::JsonSchema {
        name,
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
    if let Some(strict) = strict {
        obj.insert("strict".into(), (*strict).into());
    }
    obj.insert("schema".into(), schema.clone());
    serde_json::Value::Object(obj)
}

#[async_trait]
impl Transport for OpenAiResponsesTransport {
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
            Content::Text { text } => {
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
            } => {
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            Content::ToolResult { call_id, content } => {
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content,
                }));
            }
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

/// Render a canonical result into Responses `output` items.
fn render_output_items(result: &GenerateResult) -> Vec<serde_json::Value> {
    let mut items = Vec::new();
    let reasoning: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Reasoning { text } => Some(text.as_str()),
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
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !text.is_empty() {
        items.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    for c in &result.content {
        if let Content::ToolCall {
            id,
            name,
            arguments,
        } = c
        {
            items.push(serde_json::json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": arguments,
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
    })
}

// ===== streaming =====

/// OpenAI Responses SSE decoder. Explicit state machine over the lifecycle
/// envelope. Tracks `item_id → call_id` so `function_call_arguments.delta`
/// events map back to the canonical tool-call id (#434).
#[derive(Default)]
struct ResponsesStreamDecoder {
    /// item_id → (call_id, tool_name) for in-flight function-call items.
    function_items: Vec<(String, String, String)>,
}

impl ResponsesStreamDecoder {
    fn call_for_item(&self, item_id: &str) -> Option<(String, String)> {
        self.function_items
            .iter()
            .find(|(id, _, _)| id == item_id)
            .map(|(_, call_id, name)| (call_id.clone(), name.clone()))
    }
}

impl StreamDecoder for ResponsesStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let json: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        // The event name lives in the JSON body's `type` field; Responses also
        // mirrors it onto the SSE `event:` line, but several upstreams (notably
        // OpenRouter and stock OpenAI when fronted via gateways) emit only the
        // `data:` line — the SSE spec then defaults `event` to "message",
        // which would otherwise shadow the real event name. Always prefer the
        // body `type` and fall back to the SSE header only when it's absent.
        let event_type = json
            .get("type")
            .and_then(|t| t.as_str())
            .or(event.event.as_deref())
            .unwrap_or_default();

        let mut parts = Vec::new();
        match event_type {
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_text.done" => {}
            "response.output_item.added" => {
                if let Some(item) = json.get("item")
                    && item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                {
                    let item_id = item
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or(&item_id)
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string();
                    self.function_items
                        .push((item_id, call_id.clone(), name.clone()));
                    parts.push(StreamPart::ToolCallDelta {
                        id: call_id,
                        name: Some(name),
                        arguments: String::new(),
                    });
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                    parts.push(StreamPart::TextDelta {
                        text: delta.to_string(),
                    });
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
            "response.function_call_arguments.done" | "response.output_item.done" => {}
            "response.completed" | "response.incomplete" => {
                // #454-2: the full `response` object is carried here..3:
                // map `response.completed` to the dedicated `ResponseCompleted`
                // part so the response id + status survive (a bare `Finish`
                // would lose them).
                let response = json.get("response");
                let usage = response.and_then(|r| r.get("usage")).and_then(parse_usage);
                let id = response
                    .and_then(|r| r.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                // #432: `incomplete` is terminal-but-fine, not an error.
                let status = if event_type == "response.incomplete" {
                    "incomplete"
                } else {
                    "completed"
                };
                parts.push(StreamPart::ResponseCompleted {
                    id,
                    status: status.to_string(),
                    usage,
                });
            }
            "error" | "response.failed" => {
                // Map OpenAI's `error.type` to an HTTP status so 4xx upstream
                // errors don't always 502 here (which would trigger fallback
                // retries). Spec:
                // <https://platform.openai.com/docs/guides/error-codes>.
                let error_obj = json.get("response").and_then(|r| r.get("error"));
                let err_type = error_obj
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let msg = error_obj
                    .and_then(|e| e.get("message"))
                    .or_else(|| json.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("responses stream error");
                return Err(BitrouterError::Upstream {
                    status: openai_error_status(err_type),
                    message: msg.to_string(),
                });
            }
            // genuinely unknown, forward-compatible event — ignore (#432)
            _ => {
                tracing::debug!(event_type, "ignoring unknown responses stream event");
            }
        }
        Ok(parts)
    }
}

/// OpenAI Responses SSE encoder. Emits the complete lifecycle envelope:
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
    request_id: String,
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

    fn ensure_created(&mut self, frames: &mut Vec<SseFrame>) {
        if !self.created {
            self.created = true;
            let response = serde_json::json!({
                "id": self.request_id,
                "object": "response",
                "model": self.model,
                "status": "in_progress",
                "output": [],
            });
            // The OpenAI Responses stream opens with `response.created`
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
        let event_name = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
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
        self.ensure_created(&mut frames);
        match part {
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
                // A delta carrying a `name` starts a new function-call
                // item. `open_tool_item` closes any previously-open item
                // (reasoning / message / prior tool) first, so each tool
                // call lands in its own output slot.
                if let Some(name) = name {
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
            StreamPart::Usage { .. } => {}
            StreamPart::Finish { reason } => {
                // A bare `Finish` (e.g. inbound was OpenAI Chat / Anthropic /
                // Google) — synthesise the terminal envelope from the reason.
                let status = match reason {
                    FinishReason::Length => "incomplete",
                    FinishReason::Error(_) => "failed",
                    _ => "completed",
                };
                self.emit_terminal(&mut frames, status, &self.request_id.clone(), None);
            }
            StreamPart::ResponseCompleted { id, status, usage } => {
                // A native Responses terminal part — use the carried id/status
                //, falling back to our request id if absent.
                let response_id = if id.is_empty() {
                    self.request_id.clone()
                } else {
                    id.clone()
                };
                self.emit_terminal(&mut frames, status, &response_id, *usage);
            }
        }
        Ok(frames)
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Responses surfaces a mid-stream error as a `response.failed` event,
        // carrying the full response object with an `error` (#454-2 envelope).
        let response = serde_json::json!({
            "id": self.request_id,
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

    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        // #454-2: Responses never emits a `[DONE]` sentinel.
        Ok(Vec::new())
    }
}
