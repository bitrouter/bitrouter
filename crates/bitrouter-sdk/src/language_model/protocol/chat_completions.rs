//! Chat Completions adapter.
//!
//! Official reference: <https://platform.openai.com/docs/api-reference/chat>
//! Streaming format: <https://platform.openai.com/docs/api-reference/chat-streaming>
//!
//! Chat Completions is treated as the canonical "hub" shape — it maps most
//! directly onto the internal representation.

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

/// The Chat Completions inbound + outbound protocol adapter.
pub struct ChatCompletionsAdapter;

/// HTTP transport for Chat Completions: `POST {api_base}/chat/completions` with
/// `Authorization: Bearer <api_key>`.
pub struct ChatCompletionsTransport;

// ===== wire request types =====

/// Chat Completions request body
/// (<https://platform.openai.com/docs/api-reference/chat/create>).
///
/// `pub` so downstream crates (notably `bitrouter-cloud`) can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    tools: Vec<ChatTool>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_completion_tokens: Option<u32>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `stop` / `stop_sequences`, `seed`,
    /// `response_format`, `n`, `presence_penalty`, `frequency_penalty`,
    /// `logit_bias`, `logprobs`, `top_logprobs`, `user`, `stream_options`,
    /// `parallel_tool_calls`, … — survives parse/render via `extra`. v0
    /// passed these through; v1 must too. Skipped from the published schema
    /// so the documented contract is the set of typed fields; pass-through
    /// behavior is preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// One element of [`ChatRequest`]'s `messages` array — a chat turn carrying
/// role + optional content + optional tool calls / tool-call reply id.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatMessage {
    role: String,
    /// `content` may be a plain string or an array of content parts, or absent
    /// (an assistant turn that is purely `tool_calls`).
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
    /// Reasoning content — **not** in OpenAI's Chat Completions spec; this is
    /// a vendor extension used by DeepSeek
    /// (<https://api-docs.deepseek.com/api/create-chat-completion>), Moonshot
    /// (Kimi), Qwen, and other OpenAI-compatible providers that expose the
    /// model's chain-of-thought on the Chat envelope. v0 #454-1: it must
    /// not be dropped. `parse_response` additionally accepts `reasoning`
    /// (some OpenRouter passthroughs) and `thinking` (Aliyun) as aliases.
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// One assistant tool-call entry on a [`ChatMessage`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatToolCall {
    id: String,
    #[serde(default)]
    function: ChatFunctionCall,
}

/// The `function` payload of a [`ChatToolCall`]: name + raw JSON argument string.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ChatFunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

/// One element of [`ChatRequest`]'s `tools` array — a `{ function: { … } }` envelope.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatTool {
    #[serde(default)]
    function: ChatToolFunction,
}

/// The `function` payload of a [`ChatTool`]: name + description + JSON-Schema
/// parameters.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ChatToolFunction {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
}

// ===== role mapping (total — v0 #454-4) =====

/// Map an OpenAI role string to a canonical [`Role`]. Total mapping: an unknown
/// role is a hard error, never a silent downgrade to `User`.
fn parse_role(role: &str) -> Result<Role> {
    match role {
        "system" | "developer" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" | "function" => Ok(Role::Tool),
        other => Err(BitrouterError::bad_request(format!(
            "unknown message role '{other}' (expected system/developer/user/assistant/tool)"
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

/// Extract plain text from an OpenAI `content` value (string, or an array of
/// `{type:"text", text:"..."}` parts). Non-text parts are ignored for now.
fn content_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

impl InboundAdapter for ChatCompletionsAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::ChatCompletions
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        let req: ChatRequest = serde_json::from_value(body.clone())
            .map_err(|e| describe_deser_error("ChatRequest", &e, &body))?;

        let mut system: Option<String> = None;
        let mut messages = Vec::new();

        for m in req.messages {
            let role = parse_role(&m.role)?;
            if role == Role::System {
                let text = m.content.as_ref().map(content_text).unwrap_or_default();
                system = Some(match system {
                    Some(prev) => format!("{prev}\n{text}"),
                    None => text,
                });
                continue;
            }

            let mut content = Vec::new();
            // reasoning first so its position before text is preserved (#454-1)
            if let Some(reasoning) = m.reasoning_content
                && !reasoning.is_empty()
            {
                content.push(Content::Reasoning { text: reasoning });
            }
            if role == Role::Tool {
                let result = m.content.as_ref().map(content_text).unwrap_or_default();
                let call_id = m.tool_call_id.ok_or_else(|| {
                    BitrouterError::bad_request("tool message missing 'tool_call_id'")
                })?;
                content.push(Content::ToolResult {
                    call_id,
                    content: result,
                });
            } else {
                if let Some(text) = &m.content {
                    let text = content_text(text);
                    if !text.is_empty() {
                        content.push(Content::Text { text });
                    }
                }
                for tc in m.tool_calls {
                    content.push(Content::ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments,
                    });
                }
            }
            messages.push(Message { role, content });
        }

        let tools = req
            .tools
            .into_iter()
            .map(|t| crate::language_model::types::Tool {
                name: t.function.name,
                description: t.function.description,
                parameters: t.function.parameters,
            })
            .collect();

        // Promote `response_format: { type: "json_schema", ... }` out of the
        // extras splat into the canonical slot so cross-protocol routing can
        // translate it. The legacy `{ type: "json_object" }` JSON mode is left
        // in extras (it has no schema to translate and passes through opaquely).
        let mut extra = req.extra;
        let response_format = parse_chat_response_format(&extra);
        if response_format.is_some() {
            extra.remove("response_format");
        }

        Ok(Prompt {
            model: req.model,
            system,
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_tokens.or(req.max_completion_tokens),
                reasoning_effort: req.reasoning_effort,
                // Every Chat Completions field we don't have a typed slot for —
                // tool_choice, stop / stop_sequences, seed, n,
                // presence/frequency_penalty, logit_bias, … — rides along
                // in `extra` and is splatted back on render. Note:
                // `response_format` with `type:"json_schema"` is promoted into
                // `Prompt::response_format` above; other shapes (e.g.
                // `type:"json_object"`) stay in extras.
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
        let mut message = serde_json::Map::new();
        message.insert("role".into(), "assistant".into());

        let text: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // `content` is always present (possibly an empty string) — never null.
        message.insert("content".into(), text.into());

        let reasoning: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if !reasoning.is_empty() {
            message.insert("reasoning_content".into(), reasoning.into());
        }

        let tool_calls: Vec<_> = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                })),
                _ => None,
            })
            .collect();
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), tool_calls.into());
        }

        let mut response = serde_json::Map::new();
        response.insert("id".into(), request_id.into());
        response.insert("object".into(), "chat.completion".into());
        response.insert("model".into(), prompt.model.clone().into());
        response.insert(
            "choices".into(),
            serde_json::json!([{
                "index": 0,
                "message": serde_json::Value::Object(message),
                "finish_reason": result.finish_reason.as_ref().map(finish_reason_str),
            }]),
        );
        if let Some(usage) = result.usage {
            response.insert("usage".into(), render_usage(&usage));
        }
        Ok(serde_json::Value::Object(response))
    }

    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder> {
        Box::new(ChatStreamEncoder {
            request_id: request_id.to_string(),
            model: model.to_string(),
            role_sent: false,
            tool_call_indices: Vec::new(),
        })
    }
}

impl OutboundAdapter for ChatCompletionsAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::ChatCompletions
    }

    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value> {
        let mut messages = Vec::new();
        if let Some(system) = &prompt.system {
            messages.push(serde_json::json!({ "role": "system", "content": system }));
        }
        for m in &prompt.messages {
            messages.push(render_message(m));
        }

        let mut req = serde_json::Map::new();
        req.insert("model".into(), prompt.model.clone().into());
        req.insert("messages".into(), messages.into());
        if !prompt.tools.is_empty() {
            req.insert(
                "tools".into(),
                prompt
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
        // Absent params are omitted entirely, never serialised as null (#454-5).
        if let Some(t) = prompt.params.temperature {
            req.insert("temperature".into(), t.into());
        }
        if let Some(p) = prompt.params.top_p {
            req.insert("top_p".into(), p.into());
        }
        if let Some(mt) = prompt.params.max_tokens {
            req.insert("max_tokens".into(), mt.into());
        }
        if let Some(re) = &prompt.params.reasoning_effort {
            req.insert("reasoning_effort".into(), re.clone().into());
        }
        // Render the canonical response_format into Chat Completions' native shape.
        // Inserted before the extras splat so it wins over any legacy
        // `response_format` left in extras (typed slot wins, matching how
        // other params are handled).
        if let Some(rf) = &prompt.response_format {
            req.insert("response_format".into(), render_chat_response_format(rf));
        }
        // Splat the extras back into the outbound request — this is how
        // `tool_choice`, `stop`, `seed`, etc. survive the round trip. Typed
        // fields above win over any same-named extra.
        for (k, v) in &prompt.params.extra {
            req.entry(k.clone()).or_insert_with(|| v.clone());
        }
        req.insert("stream".into(), prompt.stream.into());
        // For streaming Chat Completions, force `stream_options.include_usage`
        // on so the trailing usage chunk arrives. Without this, OpenAI and
        // most OpenAI-compatible upstreams omit the usage frame, and the
        // pipeline's settlement layer sees zero tokens. The official ref:
        // <https://platform.openai.com/docs/api-reference/chat-streaming#chat-streaming-stream_options>.
        // Caller-supplied entries under `stream_options` are preserved.
        if prompt.stream {
            let entry = req
                .entry("stream_options".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(map) = entry.as_object_mut() {
                map.entry("include_usage".to_string())
                    .or_insert(serde_json::Value::Bool(true));
            }
        }
        Ok(serde_json::Value::Object(req))
    }

    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult> {
        let choice = body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| BitrouterError::bad_request("chat response missing 'choices[0]'"))?;
        let message = choice
            .get("message")
            .ok_or_else(|| BitrouterError::bad_request("chat choice missing 'message'"))?;

        let mut content = Vec::new();
        // `reasoning_content` is the DeepSeek-/Moonshot-/Qwen-style field name
        // for the reasoning trace on the Chat envelope. Other OpenAI-compatible
        // upstreams expose the same data under `reasoning` (some OpenRouter
        // passthroughs) or `thinking` (Aliyun). Accept whichever is present.
        // None of these are documented on Chat Completions itself; the canonical
        // Chat Completions object does not carry a reasoning trace.
        if let Some(reasoning) = ["reasoning_content", "reasoning", "thinking"]
            .iter()
            .find_map(|key| {
                message
                    .get(*key)
                    .and_then(|r| r.as_str())
                    .filter(|s| !s.is_empty())
            })
        {
            content.push(Content::Reasoning {
                text: reasoning.to_string(),
            });
        }
        if let Some(text) = message
            .get("content")
            .filter(|c| !c.is_null())
            .map(content_text)
            .filter(|s| !s.is_empty())
        {
            content.push(Content::Text { text });
        }
        // Chat Completions' `message.refusal` (when non-empty) is the model's
        // declined-response text. Carry it through so the caller sees the
        // refusal rather than an empty assistant message. Spec:
        // <https://platform.openai.com/docs/api-reference/chat/object>.
        let mut refusal_seen = false;
        if let Some(refusal) = message
            .get("refusal")
            .and_then(|r| r.as_str())
            .filter(|s| !s.is_empty())
        {
            content.push(Content::Text {
                text: refusal.to_string(),
            });
            refusal_seen = true;
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                content.push(Content::ToolCall {
                    id: tc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }

        let finish_reason = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .and_then(parse_finish_reason)
            // A `refusal` field signals content-filter regardless of what
            // `finish_reason` claims, so the caller can surface the refusal.
            .map(|fr| {
                if refusal_seen {
                    FinishReason::ContentFilter
                } else {
                    fr
                }
            });
        let usage = body.get("usage").and_then(parse_usage);
        // Chat Completions: top-level `id` (`chatcmpl-...`).
        // <https://platform.openai.com/docs/api-reference/chat/object>
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
        Box::new(ChatStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

/// Detect a `response_format: { type: "json_schema", json_schema: {...} }`
/// blob in the inbound extras and lift it into the canonical [`ResponseFormat`].
/// Returns `None` for any other shape (notably `{ type: "json_object" }`,
/// which is the older JSON-mode and has no schema to translate).
fn parse_chat_response_format(
    extra: &HashMap<String, serde_json::Value>,
) -> Option<ResponseFormat> {
    let rf = extra.get("response_format")?;
    if rf.get("type")?.as_str()? != "json_schema" {
        return None;
    }
    let js = rf.get("json_schema")?;
    let schema = js.get("schema")?.clone();
    Some(ResponseFormat::JsonSchema {
        name: js
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string()),
        strict: js.get("strict").and_then(|s| s.as_bool()),
        schema,
    })
}

/// Render a canonical [`ResponseFormat`] into Chat Completions' native
/// `{ type: "json_schema", json_schema: { name, strict, schema } }`. OpenAI
/// requires `name`; supply a stable default when the caller didn't set one.
fn render_chat_response_format(rf: &ResponseFormat) -> serde_json::Value {
    let ResponseFormat::JsonSchema {
        name,
        strict,
        schema,
    } = rf;
    let mut js = serde_json::Map::new();
    js.insert(
        "name".into(),
        name.clone()
            .unwrap_or_else(|| "response".to_string())
            .into(),
    );
    if let Some(strict) = strict {
        js.insert("strict".into(), (*strict).into());
    }
    js.insert("schema".into(), schema.clone());
    serde_json::json!({ "type": "json_schema", "json_schema": serde_json::Value::Object(js) })
}

#[async_trait]
impl Transport for ChatCompletionsTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::ChatCompletions
    }

    fn endpoint_url(&self, target: &RoutingTarget, _stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        format!("{base}/chat/completions")
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

fn render_message(m: &Message) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), role_str(m.role).into());

    if m.role == Role::Tool {
        // tool messages carry a tool_call_id + flat string content
        for c in &m.content {
            if let Content::ToolResult { call_id, content } = c {
                obj.insert("tool_call_id".into(), call_id.clone().into());
                obj.insert("content".into(), content.clone().into());
            }
        }
        return serde_json::Value::Object(obj);
    }

    let text: String = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    obj.insert("content".into(), text.into());

    let reasoning: String = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Reasoning { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !reasoning.is_empty() {
        obj.insert("reasoning_content".into(), reasoning.into());
    }

    let tool_calls: Vec<_> = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
            } => Some(serde_json::json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            _ => None,
        })
        .collect();
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".into(), tool_calls.into());
    }
    serde_json::Value::Object(obj)
}

fn parse_finish_reason(s: &str) -> Option<FinishReason> {
    match s {
        "stop" | "end_turn" => Some(FinishReason::Stop),
        "length" | "max_tokens" => Some(FinishReason::Length),
        "tool_calls" | "function_call" => Some(FinishReason::ToolCalls),
        "content_filter" => Some(FinishReason::ContentFilter),
        other => Some(FinishReason::Other(other.to_string())),
    }
}

fn finish_reason_str(r: &FinishReason) -> String {
    match r {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool_calls".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::Other(s) => s.clone(),
        FinishReason::Error(_) => "stop".to_string(),
    }
}

fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    let prompt_tokens = value.get("prompt_tokens")?.as_u64()?;
    let completion_tokens = value
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = value
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Chat Completions surfaces cached prompt tokens under
    // `prompt_tokens_details.cached_tokens` — subset of `prompt_tokens`. Ref:
    // <https://platform.openai.com/docs/api-reference/chat/object> →
    // `usage` object. Some OpenAI-compatible providers (e.g. DeepSeek)
    // expose the same field name so we cover them too.
    let cache_read = value
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        reasoning_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
    })
}

fn render_usage(usage: &Usage) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total(),
    });
    if usage.reasoning_tokens > 0 {
        obj["completion_tokens_details"] =
            serde_json::json!({ "reasoning_tokens": usage.reasoning_tokens });
    }
    if usage.cache_read_tokens > 0 {
        obj["prompt_tokens_details"] =
            serde_json::json!({ "cached_tokens": usage.cache_read_tokens });
    }
    obj
}

// ===== streaming =====

/// Decodes Chat Completions `data:` SSE chunks into canonical stream parts. Explicit
/// state machine — unknown chunk shapes are ignored, never panicked on.
#[derive(Default)]
struct ChatStreamDecoder {
    /// Accumulates tool-call name per index so the canonical
    /// `ToolCallDelta.id` is stable across chunks.
    tool_ids: Vec<(String, String)>,
    done: bool,
    /// Whether the one-shot [`StreamPart::ResponseStarted`] has been emitted.
    /// Every chunk repeats the top-level `id`; we surface it only once.
    response_started_emitted: bool,
}

impl StreamDecoder for ChatStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let chunk: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            // A non-JSON keepalive / comment line — ignore, do not error.
            Err(_) => return Ok(Vec::new()),
        };

        let mut parts = Vec::new();
        // Surface the upstream response id once, before any deltas. Every
        // chunk repeats the top-level `id` (`chatcmpl-...`); we emit it a
        // single time for observability. Not in the Chat Completions object's
        // delta — purely the envelope id.
        // <https://platform.openai.com/docs/api-reference/chat/streaming>
        if !self.response_started_emitted
            && let Some(id) = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        {
            self.response_started_emitted = true;
            parts.push(StreamPart::ResponseStarted { id: id.to_string() });
        }
        if let Some(choice) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(|c| c.as_str())
                    && !text.is_empty()
                {
                    parts.push(StreamPart::TextDelta {
                        text: text.to_string(),
                    });
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str())
                    && !reasoning.is_empty()
                {
                    parts.push(StreamPart::ReasoningDelta {
                        text: reasoning.to_string(),
                    });
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let id = tc.get("id").and_then(|i| i.as_str());
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str());
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");
                        while self.tool_ids.len() <= idx {
                            self.tool_ids.push((String::new(), String::new()));
                        }
                        if let Some(id) = id {
                            self.tool_ids[idx].0 = id.to_string();
                        }
                        if let Some(name) = name {
                            self.tool_ids[idx].1 = name.to_string();
                        }
                        parts.push(StreamPart::ToolCallDelta {
                            id: self.tool_ids[idx].0.clone(),
                            name: name.map(|n| n.to_string()),
                            arguments: args.to_string(),
                        });
                    }
                }
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(|f| f.as_str())
                .and_then(parse_finish_reason)
            {
                if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
                    parts.push(StreamPart::Usage { usage });
                }
                parts.push(StreamPart::Finish { reason });
            }
        } else if let Some(usage) = chunk.get("usage").and_then(parse_usage) {
            // Some providers send a trailing usage-only chunk.
            parts.push(StreamPart::Usage { usage });
        }
        Ok(parts)
    }
}

/// Encodes canonical stream parts into Chat Completions `data:` SSE chunks.
struct ChatStreamEncoder {
    request_id: String,
    model: String,
    role_sent: bool,
    /// Tool-call id → its `tool_calls[].index`, assigned in first-seen order so
    /// parallel tool calls get distinct, stable indices.
    tool_call_indices: Vec<String>,
}

impl ChatStreamEncoder {
    /// Index for a tool-call id, assigning the next free slot on first sight.
    fn tool_call_index(&mut self, id: &str) -> usize {
        if let Some(pos) = self.tool_call_indices.iter().position(|x| x == id) {
            pos
        } else {
            self.tool_call_indices.push(id.to_string());
            self.tool_call_indices.len() - 1
        }
    }
}

impl ChatStreamEncoder {
    fn chunk(&self, delta: serde_json::Value, finish: Option<&str>) -> SseFrame {
        let data = serde_json::json!({
            "id": self.request_id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        SseFrame::Event {
            event: None,
            data: data.to_string(),
        }
    }
}

impl ChatStreamEncoder {
    /// Build a fresh `delta` map, injecting the one-shot `role: assistant`
    /// marker on the first call. Only the arms that actually emit a chunk
    /// call this, so a no-op part (`Usage` / `ResponseStarted`) never
    /// consumes the role marker — it must ride the first real content chunk.
    fn open_delta(&mut self) -> serde_json::Map<String, serde_json::Value> {
        let mut delta = serde_json::Map::new();
        if !self.role_sent {
            delta.insert("role".into(), "assistant".into());
            self.role_sent = true;
        }
        delta
    }
}

impl StreamEncoder for ChatStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        match part {
            StreamPart::TextDelta { text } => {
                let mut delta = self.open_delta();
                delta.insert("content".into(), text.clone().into());
                frames.push(self.chunk(serde_json::Value::Object(delta), None));
            }
            StreamPart::ReasoningDelta { text } => {
                let mut delta = self.open_delta();
                delta.insert("reasoning_content".into(), text.clone().into());
                frames.push(self.chunk(serde_json::Value::Object(delta), None));
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                let index = self.tool_call_index(id);
                let mut function = serde_json::Map::new();
                if let Some(name) = name {
                    function.insert("name".into(), name.clone().into());
                }
                function.insert("arguments".into(), arguments.clone().into());
                let mut delta = self.open_delta();
                delta.insert(
                    "tool_calls".into(),
                    serde_json::json!([{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": serde_json::Value::Object(function),
                    }]),
                );
                frames.push(self.chunk(serde_json::Value::Object(delta), None));
            }
            StreamPart::Usage { .. } => {
                // usage is attached to the Finish chunk below; nothing here.
            }
            StreamPart::ResponseStarted { .. } => {
                // Observability-only metadata (upstream response id); not
                // forwarded to the client. Crucially this arm does NOT call
                // `open_delta`, so the role marker still rides the first real
                // content chunk even when `ResponseStarted` arrives first.
            }
            StreamPart::Finish { reason } => {
                let delta = self.open_delta();
                let reason_str = finish_reason_str(reason);
                frames.push(self.chunk(serde_json::Value::Object(delta), Some(&reason_str)));
            }
            StreamPart::ResponseCompleted { status, .. } => {
                // Inbound was Responses; Chat has no response-completed
                // concept — terminate with a finish chunk derived from status.
                let reason = if status == "incomplete" {
                    FinishReason::Length
                } else {
                    FinishReason::Stop
                };
                let delta = self.open_delta();
                let reason_str = finish_reason_str(&reason);
                frames.push(self.chunk(serde_json::Value::Object(delta), Some(&reason_str)));
            }
        }
        Ok(frames)
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Chat Completions surfaces a mid-stream error as a chunk carrying an `error`
        // object, followed by the `[DONE]` sentinel.
        vec![
            SseFrame::Event {
                event: None,
                data: serde_json::json!({
                    "error": { "message": message, "type": "upstream_error" }
                })
                .to_string(),
            },
            SseFrame::Event {
                event: None,
                data: "[DONE]".to_string(),
            },
        ]
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        // Chat Completions terminates the stream with a literal `[DONE]` sentinel.
        Ok(vec![SseFrame::Event {
            event: None,
            data: "[DONE]".to_string(),
        }])
    }
}
