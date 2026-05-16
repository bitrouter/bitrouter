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

use async_trait::async_trait;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, SseEvent, StreamDecoder, StreamEncoder, Transport,
    describe_deser_error,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message, Prompt, Role,
    RoutingTarget, StreamPart, Usage,
};

/// The OpenAI Responses protocol adapter.
pub struct OpenAiResponsesAdapter;

/// HTTP transport for OpenAI Responses: `POST {api_base}/responses` with
/// `Authorization: Bearer <api_key>`.
pub struct OpenAiResponsesTransport;

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
        // Parsed field-by-field (not via a strict struct) so unexpected
        // Codex-style item shapes never cause a hard 400 (#454-3).
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let input = body.get("input").ok_or_else(|| {
            describe_deser_error(
                "ResponsesRequest",
                &serde::de::Error::missing_field("input"),
                &body,
            )
        })?;
        let messages = parse_input(input)?;
        let system = body
            .get("instructions")
            .and_then(|i| i.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let tools = body
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|t| {
                        t.get("type").and_then(|ty| ty.as_str()) == Some("function")
                            || t.get("name").is_some()
                    })
                    .map(|t| crate::language_model::types::Tool {
                        name: t
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        description: t
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(|s| s.to_string()),
                        parameters: t
                            .get("parameters")
                            .cloned()
                            .unwrap_or(serde_json::json!({})),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Collect every Responses-API field we don't have a typed slot for —
        // tool_choice, parallel_tool_calls, max_tool_calls, metadata,
        // include[], previous_response_id, store, stream_options, etc. — into
        // `extra` so render_request can splat them back. The parser walks the
        // body field-by-field (#454-3) so we explicitly subtract the known set.
        const KNOWN: &[&str] = &[
            "model",
            "input",
            "instructions",
            "tools",
            "temperature",
            "top_p",
            "max_output_tokens",
            "reasoning",
            "stream",
        ];
        let extra: std::collections::HashMap<String, serde_json::Value> = body
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Prompt {
            model,
            system,
            messages,
            tools,
            params: GenerationParams {
                temperature: body.get("temperature").and_then(|t| t.as_f64()),
                top_p: body.get("top_p").and_then(|t| t.as_f64()),
                max_tokens: body
                    .get("max_output_tokens")
                    .and_then(|m| m.as_u64())
                    .map(|m| m as u32),
                reasoning_effort: body
                    .get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string()),
                extra,
            },
            stream: body
                .get("stream")
                .and_then(|s| s.as_bool())
                .unwrap_or(false),
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
            output_index: 0,
            text_item_open: false,
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
        // The event name lives in the `type` field (Responses puts it in both
        // the SSE `event:` line and the JSON body).
        let event_type = event
            .event
            .as_deref()
            .or_else(|| json.get("type").and_then(|t| t.as_str()))
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
                if let Some(item) = json.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
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
                if let Some((call_id, _)) = self.call_for_item(item_id) {
                    if let Some(delta) = json.get("delta").and_then(|d| d.as_str()) {
                        parts.push(StreamPart::ToolCallDelta {
                            id: call_id,
                            name: None,
                            arguments: delta.to_string(),
                        });
                    }
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

/// OpenAI Responses SSE encoder. Emits the complete lifecycle envelope with a
/// monotonically increasing `sequence_number` on every event, and a
/// `response.completed` carrying the full `response` object. Never emits
/// `[DONE]` (#454-2).
struct ResponsesStreamEncoder {
    request_id: String,
    model: String,
    seq: u64,
    created: bool,
    output_index: u64,
    text_item_open: bool,
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
            frames.push(self.ev(
                "response.created",
                serde_json::json!({ "response": response }),
            ));
        }
    }

    /// Emit the terminal lifecycle frame — `response.completed` (or
    /// `response.incomplete`), carrying the full `response` object (#454-2).
    /// Shared by the `Finish` and `ResponseCompleted` encode arms.
    fn emit_terminal(
        &mut self,
        frames: &mut Vec<SseFrame>,
        status: &str,
        response_id: &str,
        usage: Option<Usage>,
    ) {
        if self.text_item_open {
            let idx = self.output_index;
            frames.push(self.ev(
                "response.output_item.done",
                serde_json::json!({ "output_index": idx }),
            ));
            self.text_item_open = false;
        }
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
                if !self.text_item_open {
                    self.text_item_open = true;
                    let idx = self.output_index;
                    frames.push(self.ev(
                        "response.output_item.added",
                        serde_json::json!({
                            "output_index": idx,
                            "item": { "type": "message", "role": "assistant", "content": [] },
                        }),
                    ));
                }
                let idx = self.output_index;
                frames.push(self.ev(
                    "response.output_text.delta",
                    serde_json::json!({ "output_index": idx, "delta": text }),
                ));
            }
            StreamPart::ReasoningDelta { text } => {
                let idx = self.output_index;
                frames.push(self.ev(
                    "response.reasoning_summary_text.delta",
                    serde_json::json!({ "output_index": idx, "delta": text }),
                ));
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                if let Some(name) = name {
                    if self.text_item_open {
                        let idx = self.output_index;
                        frames.push(self.ev(
                            "response.output_item.done",
                            serde_json::json!({ "output_index": idx }),
                        ));
                        self.text_item_open = false;
                        self.output_index += 1;
                    }
                    let idx = self.output_index;
                    frames.push(self.ev(
                        "response.output_item.added",
                        serde_json::json!({
                            "output_index": idx,
                            "item": {
                                "type": "function_call",
                                "id": id,
                                "call_id": id,
                                "name": name,
                            },
                        }),
                    ));
                }
                if !arguments.is_empty() {
                    let idx = self.output_index;
                    frames.push(self.ev(
                        "response.function_call_arguments.delta",
                        serde_json::json!({
                            "output_index": idx,
                            "item_id": id,
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
