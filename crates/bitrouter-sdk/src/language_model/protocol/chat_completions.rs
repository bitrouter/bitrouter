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
use serde::{Deserialize, Serialize};

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, PROVIDER_ID_OPENAI, SseEvent, StreamDecoder, StreamEncoder,
    Transport, describe_deser_error, rendered_finish_reason, stash_raw_finish_reason,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, DataContent, FinishReason, GenerateResult, GenerationParams, Message,
    Modality, Prompt, ProviderMetadata, ResponseFormat, Role, RoutingTarget, Source, StreamPart,
    Tool, ToolChoice, ToolResultContentPart, ToolResultOutput, Usage, provider_namespace,
    set_provider_metadata,
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
    /// Output-format constraint. The `json_schema` variant is the
    /// structured-output contract; `json_object` is the legacy JSON mode and
    /// `text` is the default. Promoted to the canonical `response_format` slot
    /// at parse time (`json_object` / `text` pass through opaquely).
    /// <https://platform.openai.com/docs/guides/structured-outputs>
    #[serde(default)]
    response_format: Option<ChatResponseFormat>,
    /// Deterministic-sampling seed. Promoted to the canonical `seed` slot so it
    /// translates across protocols (e.g. to a Gemini upstream's
    /// `generationConfig.seed`) instead of no-op'ing as a top-level key on a
    /// nested-config wire.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-seed>
    #[serde(default)]
    seed: Option<i64>,
    /// Stop sequences — a single string or an array of up to four. Promoted to
    /// the canonical `stop` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-stop>
    #[serde(default)]
    stop: Option<serde_json::Value>,
    /// Presence penalty. Promoted to the canonical `presence_penalty` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-presence_penalty>
    #[serde(default)]
    presence_penalty: Option<f64>,
    /// Frequency penalty. Promoted to the canonical `frequency_penalty` slot.
    /// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-frequency_penalty>
    #[serde(default)]
    frequency_penalty: Option<f64>,
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `n`, `logit_bias`, `logprobs`,
    /// `top_logprobs`, `user`, `stream_options`, `parallel_tool_calls`, … —
    /// survives parse/render via `extra`. v0 passed these through; v1 must too.
    /// Skipped from the published schema so the documented contract is the set
    /// of typed fields; pass-through behavior is preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// Chat Completions `response_format`
/// (<https://platform.openai.com/docs/guides/structured-outputs>) — a closed
/// union over OpenAI's three output modes.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatResponseFormat {
    /// Free-form text — the default.
    Text,
    /// Legacy JSON mode: valid JSON with no schema.
    JsonObject,
    /// JSON constrained to a schema.
    JsonSchema {
        /// The schema spec.
        json_schema: ChatJsonSchema,
    },
}

/// The `json_schema` payload of [`ChatResponseFormat::JsonSchema`].
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ChatJsonSchema {
    /// Schema name (OpenAI requires it).
    name: String,
    /// Optional schema description — extra LLM guidance OpenAI passes through to
    /// the model. Promoted to the canonical `response_format` description.
    /// <https://platform.openai.com/docs/guides/structured-outputs>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Strict-mode flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
    /// The JSON Schema.
    schema: serde_json::Value,
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
/// parameters + optional `strict` flag.
/// <https://platform.openai.com/docs/guides/function-calling#strict-mode>
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ChatToolFunction {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
    /// OpenAI strict-mode flag (V3 `strict`). Captured so it is not lost across
    /// the canonical boundary.
    #[serde(default)]
    strict: Option<bool>,
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

/// Parse an OpenAI `tool` message `content` value into a canonical
/// [`ToolResultOutput`]. A string → [`ToolResultOutput::Text`]. A content-part
/// array that carries any media part → [`ToolResultOutput::Content`] (text +
/// media, in order); a text-only array collapses to `Text` so a trivial
/// `[{type:text}]` does not gratuitously promote to the multimodal variant. Any
/// other JSON value → [`ToolResultOutput::Json`].
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
fn parse_tool_result_output(value: &serde_json::Value) -> ToolResultOutput {
    match value {
        serde_json::Value::String(s) => ToolResultOutput::Text { value: s.clone() },
        serde_json::Value::Array(parts) => {
            let canonical: Vec<Content> = parts.iter().filter_map(parse_chat_part).collect();
            let has_media = canonical.iter().any(|c| matches!(c, Content::File { .. }));
            if has_media {
                let value = canonical
                    .into_iter()
                    .filter_map(content_to_tool_result_part)
                    .collect();
                ToolResultOutput::Content { value }
            } else {
                ToolResultOutput::Text {
                    value: content_text(value),
                }
            }
        }
        other => ToolResultOutput::from_untyped_value(other),
    }
}

/// Map a canonical text/file [`Content`] into a [`ToolResultContentPart`].
/// Only text and media parts have a tool-result-content representation; any
/// other content kind yields `None`.
fn content_to_tool_result_part(c: Content) -> Option<ToolResultContentPart> {
    match c {
        Content::Text { text, .. } => Some(ToolResultContentPart::Text { text }),
        Content::File {
            media_type, data, ..
        } => Some(ToolResultContentPart::Media { media_type, data }),
        _ => None,
    }
}

/// Parse an OpenAI `content` value (string, or array of content parts) into
/// ordered canonical content. Text + media parts are preserved in order; other
/// part shapes are skipped.
fn parse_chat_content(value: &serde_json::Value) -> Vec<Content> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => vec![Content::Text {
            text: s.clone(),
            provider_metadata: ProviderMetadata::new(),
        }],
        serde_json::Value::Array(parts) => parts.iter().filter_map(parse_chat_part).collect(),
        _ => Vec::new(),
    }
}

/// Parse one OpenAI content part into canonical content.
fn parse_chat_part(part: &serde_json::Value) -> Option<Content> {
    match part.get("type").and_then(|t| t.as_str())? {
        "text" => {
            let text = part.get("text").and_then(|t| t.as_str())?.to_string();
            (!text.is_empty()).then_some(Content::Text {
                text,
                provider_metadata: ProviderMetadata::new(),
            })
        }
        // <https://platform.openai.com/docs/guides/vision>
        "image_url" => {
            let image_url = part.get("image_url")?;
            let url = image_url.get("url").and_then(|u| u.as_str())?;
            let (media_type, data) = DataContent::from_url(url);
            // The OpenAI image `detail` hint (`auto` | `low` | `high`) is
            // provider metadata, not a payload field — preserve it under the
            // `openai` namespace so it survives a round-trip (and is ignored,
            // not leaked, on a non-OpenAI upstream).
            // <https://platform.openai.com/docs/guides/vision>
            let mut provider_metadata = ProviderMetadata::new();
            if let Some(detail) = image_url.get("detail") {
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
        // <https://platform.openai.com/docs/guides/audio>
        "input_audio" => {
            let audio = part.get("input_audio")?;
            let format = audio
                .get("format")
                .and_then(|f| f.as_str())
                .unwrap_or("mp3");
            let data = if let Some(d) = audio.get("data").and_then(|d| d.as_str()) {
                DataContent::Base64 {
                    data: d.to_string(),
                }
            } else {
                DataContent::Url {
                    url: audio.get("url").and_then(|u| u.as_str())?.to_string(),
                }
            };
            Some(Content::File {
                media_type: format!("audio/{format}"),
                data,
                filename: None,
                provider_metadata: ProviderMetadata::new(),
            })
        }
        "file" => {
            let file = part.get("file")?;
            let file_data = file.get("file_data").and_then(|d| d.as_str())?;
            let (media_type, data) = DataContent::from_url(file_data);
            Some(Content::File {
                media_type: media_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                data,
                filename: file
                    .get("filename")
                    .and_then(|f| f.as_str())
                    .map(str::to_string),
                provider_metadata: ProviderMetadata::new(),
            })
        }
        _ => None,
    }
}

/// Parse an OpenAI Chat `message.annotations[]` array into canonical
/// [`Content::Source`] parts. Only `url_citation` entries
/// (`{type:"url_citation", url_citation:{url, title?}}`) are mapped — the only
/// annotation kind Chat Completions emits for web search. The citation id is
/// synthesized from the url + index (the wire carries none); the `start_index`/
/// `end_index` text offsets are dropped (no canonical slot). Mirrors the AI SDK
/// OpenAI chat mapping.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/chat/openai-chat-language-model.ts>
fn parse_chat_annotations(annotations: Option<&serde_json::Value>) -> Vec<Content> {
    let Some(arr) = annotations.and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .filter_map(|(i, ann)| {
            if ann.get("type").and_then(|t| t.as_str()) != Some("url_citation") {
                return None;
            }
            let cite = ann.get("url_citation")?;
            let url = cite.get("url").and_then(|u| u.as_str())?.to_string();
            let title = cite
                .get("title")
                .and_then(|t| t.as_str())
                .map(str::to_string);
            Some(Content::Source {
                source: Source::Url {
                    id: Source::synthesize_id(&url, i),
                    url,
                    title,
                },
                provider_metadata: ProviderMetadata::new(),
            })
        })
        .collect()
}

/// Render canonical [`Content::Source`] parts back into OpenAI Chat
/// `message.annotations[]` `url_citation` entries. Only [`Source::Url`] has a
/// Chat representation; a [`Source::Document`] citation (which only OpenAI
/// Responses / Anthropic documents produce) has no `url_citation` shape on this
/// wire and is dropped — documented cross-protocol loss. Returns an empty Vec
/// when the result carries no URL sources.
/// <https://platform.openai.com/docs/api-reference/chat/object> (`annotations`)
fn render_chat_annotations(result: &GenerateResult) -> Vec<serde_json::Value> {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Source {
                source: Source::Url { url, title, .. },
                ..
            } => {
                let mut cite = serde_json::Map::new();
                cite.insert("url".into(), url.clone().into());
                if let Some(title) = title {
                    cite.insert("title".into(), title.clone().into());
                }
                Some(serde_json::json!({
                    "type": "url_citation",
                    "url_citation": serde_json::Value::Object(cite),
                }))
            }
            // Document citations have no `url_citation` form on the Chat wire.
            Content::Source {
                source: Source::Document { .. },
                ..
            } => None,
            _ => None,
        })
        .collect()
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
                content.push(Content::Reasoning {
                    text: reasoning,
                    provider_metadata: ProviderMetadata::new(),
                });
            }
            if role == Role::Tool {
                // OpenAI tool messages carry `{role:"tool", tool_call_id, content}`;
                // `content` is a string or a content-part array, with no tool
                // name and no error flag on the wire. A string → Text, a part
                // array carrying media → Content, any other structured value →
                // Json.
                // <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
                let output = m
                    .content
                    .as_ref()
                    .map(parse_tool_result_output)
                    .unwrap_or_else(|| ToolResultOutput::Text {
                        value: String::new(),
                    });
                let call_id = m.tool_call_id.ok_or_else(|| {
                    BitrouterError::bad_request("tool message missing 'tool_call_id'")
                })?;
                content.push(Content::ToolResult {
                    call_id,
                    tool_name: None,
                    output,
                    // Chat Completions has no MCP tool-result wire.
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                });
            } else {
                if let Some(value) = &m.content {
                    content.extend(parse_chat_content(value));
                }
                for tc in m.tool_calls {
                    content.push(Content::ToolCall {
                        id: tc.id,
                        name: tc.function.name,
                        arguments: tc.function.arguments,
                        // Chat Completions `tool_calls` are always client tools —
                        // there is no server-tool slot on this wire.
                        provider_executed: false,
                        // …and no provider-executed MCP (`dynamic`) call envelope.
                        dynamic: false,
                        provider_metadata: ProviderMetadata::new(),
                    });
                }
            }
            messages.push(Message { role, content });
        }

        // Chat Completions is function-only on the wire — there is no
        // provider-defined ("server") tool envelope, so every entry parses to a
        // `Tool::Function`. The `strict` flag is captured into the canonical slot
        // (previously dropped here).
        // <https://platform.openai.com/docs/api-reference/chat/create#chat-create-tools>
        let tools = req
            .tools
            .into_iter()
            .map(|t| crate::language_model::types::Tool::Function {
                name: t.function.name,
                description: t.function.description,
                parameters: t.function.parameters,
                strict: t.function.strict,
                // Chat Completions has no per-tool `cache_control`; no provider
                // metadata to lift here.
                provider_metadata: ProviderMetadata::new(),
            })
            .collect();

        // `response_format` is a typed field. Promote `json_schema` into the
        // canonical slot so cross-protocol routing can translate it; `json_object`
        // / `text` carry no schema, so re-attach them to `extra` to pass through
        // opaquely on render (v0 parity).
        let mut extra = req.extra;
        let response_format = match req.response_format {
            Some(ChatResponseFormat::JsonSchema { json_schema }) => {
                Some(ResponseFormat::JsonSchema {
                    name: Some(json_schema.name),
                    description: json_schema.description,
                    strict: json_schema.strict,
                    schema: json_schema.schema,
                })
            }
            Some(other) => {
                if let Ok(value) = serde_json::to_value(&other) {
                    extra.insert("response_format".to_string(), value);
                }
                None
            }
            None => None,
        };

        // Output modalities (OpenAI `modalities`, e.g. ["text","audio"]). Promote
        // to the typed slot so capability detection sees them; remove from extra
        // so they are not double-rendered.
        let response_modalities = extra
            .remove("modalities")
            .and_then(|v| serde_json::from_value::<Vec<Modality>>(v).ok())
            .unwrap_or_default();

        // Promote a known-shape `tool_choice` into the canonical slot so it can
        // translate across protocols (the v0 #547 bug: Anthropic's object form
        // reaching an OpenAI upstream). Unmapped shapes stay in `extra`.
        let tool_choice = parse_chat_tool_choice(&mut extra);

        Ok(Prompt {
            model: req.model,
            system,
            // Chat Completions has no system-level `cache_control` on its wire.
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_tokens.or(req.max_completion_tokens),
                reasoning_effort: req.reasoning_effort,
                response_modalities,
                // Chat Completions carries no top-k.
                top_k: None,
                seed: req.seed,
                stop: parse_chat_stop(req.stop),
                presence_penalty: req.presence_penalty,
                frequency_penalty: req.frequency_penalty,
                // Every remaining Chat Completions field without a typed slot —
                // n, logit_bias, … — rides in `extra` and is splatted back on
                // render.
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
        let mut message = serde_json::Map::new();
        message.insert("role".into(), "assistant".into());

        // `content` is a plain string for text-only replies; when the reply
        // carries a generated file (e.g. an image), emit a parts array — the same
        // shape OpenAI uses for input media. Non-standard for a chat *response*,
        // but it preserves the data rather than dropping it.
        if result
            .content
            .iter()
            .any(|c| matches!(c, Content::File { .. }))
        {
            let parts: Vec<serde_json::Value> = result
                .content
                .iter()
                .filter_map(render_input_part)
                .collect();
            message.insert("content".into(), parts.into());
        } else {
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            // `content` is always present (possibly an empty string) — never null.
            message.insert("content".into(), text.into());
        }

        let reasoning: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Reasoning { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if !reasoning.is_empty() {
            message.insert("reasoning_content".into(), reasoning.into());
        }

        // Chat Completions has no provider-executed server-tool slot, so a
        // `provider_executed` call degrades to a plain `tool_calls` entry here
        // (the flag is dropped) — `..` ignores it deliberately.
        let tool_calls: Vec<_> = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
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

        // Re-attach web-search citations as `message.annotations[]`
        // `url_citation` entries — the same location `parse_response` lifts them
        // from. Collected from the result's `Content::Source` parts rather than
        // rendered per-part (citations are response annotations, not content).
        // <https://platform.openai.com/docs/api-reference/chat/object> (`annotations`)
        let annotations = render_chat_annotations(result);
        if !annotations.is_empty() {
            message.insert("annotations".into(), annotations.into());
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
                // Prefer the stashed raw finish reason (e.g. `function_call`)
                // over the unified-enum mapping so a same-protocol round-trip
                // reproduces the exact native value.
                "finish_reason": rendered_finish_reason(result, PROVIDER_ID_OPENAI, finish_reason_str),
            }]),
        );
        if let Some(usage) = result.usage {
            response.insert("usage".into(), render_usage(&usage));
        }
        // Restore the OpenAI `system_fingerprint` from result-level provider
        // metadata when present (only ever set by this protocol's
        // `parse_response`), so a same-protocol round-trip reproduces it.
        // <https://platform.openai.com/docs/api-reference/chat/object>
        if let Some(fp) = provider_namespace(&result.provider_metadata, PROVIDER_ID_OPENAI)
            .and_then(|o| o.get("systemFingerprint"))
        {
            response.insert("system_fingerprint".into(), fp.clone());
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
        // Drop tools with no Chat Completions wire form (provider-defined /
        // server tools); only emit `tools` if at least one function tool remains,
        // so a request that carried only server tools doesn't send `tools: []`.
        let tools: Vec<_> = prompt.tools.iter().filter_map(render_chat_tool).collect();
        if !tools.is_empty() {
            req.insert("tools".into(), tools.into());
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
        // Output modalities -> OpenAI `modalities`.
        if !prompt.params.response_modalities.is_empty()
            && let Ok(value) = serde_json::to_value(&prompt.params.response_modalities)
        {
            req.insert("modalities".into(), value);
        }
        // Render the canonical tool_choice into Chat Completions' native shape.
        // Inserted before the extras splat so it wins over any leftover
        // `tool_choice` (matching how response_format is handled).
        if let Some(tc) = &prompt.tool_choice {
            req.insert("tool_choice".into(), render_chat_tool_choice(tc));
        }
        // Render the typed sampling slots into their Chat Completions wire names.
        // Chat Completions carries no top-k, so `top_k` is intentionally not
        // rendered here (it reaches Anthropic/Gemini upstreams instead). `stop`
        // renders as an array, which the API accepts for any count.
        // <https://platform.openai.com/docs/api-reference/chat/create>
        if let Some(seed) = prompt.params.seed {
            req.insert("seed".into(), seed.into());
        }
        if !prompt.params.stop.is_empty() {
            req.insert("stop".into(), prompt.params.stop.clone().into());
        }
        if let Some(pp) = prompt.params.presence_penalty {
            req.insert("presence_penalty".into(), pp.into());
        }
        if let Some(fp) = prompt.params.frequency_penalty {
            req.insert("frequency_penalty".into(), fp.into());
        }
        // Splat the extras back into the outbound request — this is how
        // remaining untyped fields (`parallel_tool_calls`, n, logit_bias, …)
        // survive the round trip. Typed fields above win over any same-named
        // extra.
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
                provider_metadata: ProviderMetadata::new(),
            });
        }
        if let Some(text) = message
            .get("content")
            .filter(|c| !c.is_null())
            .map(content_text)
            .filter(|s| !s.is_empty())
        {
            content.push(Content::Text {
                text,
                provider_metadata: ProviderMetadata::new(),
            });
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
                provider_metadata: ProviderMetadata::new(),
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
                    // The Chat Completions response wire has no server-tool item;
                    // every `tool_calls` entry is a client tool call.
                    provider_executed: false,
                    // …and no provider-executed MCP (`dynamic`) call envelope.
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                });
            }
        }
        // Web-search citations ride `message.annotations[]` as `url_citation`
        // entries `{type:"url_citation", url_citation:{url, title, ...}}`. Lift
        // each into a canonical `Content::Source` (URL) so it is not dropped.
        // The wire carries no citation id, so synthesize a stable one from the
        // url + index. `start_index`/`end_index` (the text offsets) have no slot
        // on the canonical `Source` and are dropped — see [`Source`].
        // <https://platform.openai.com/docs/api-reference/chat/object> (`annotations`)
        content.extend(parse_chat_annotations(message.get("annotations")));

        let raw_finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(str::to_string);
        let finish_reason = raw_finish
            .as_deref()
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
        // OpenAI's `system_fingerprint` identifies the backend config the
        // response was produced with — it has no dedicated canonical field, so
        // carry it at result level under the `openai` namespace.
        // <https://platform.openai.com/docs/api-reference/chat/object>
        let mut provider_metadata = ProviderMetadata::new();
        if let Some(fp) = body.get("system_fingerprint").filter(|v| !v.is_null()) {
            set_provider_metadata(
                &mut provider_metadata,
                PROVIDER_ID_OPENAI,
                "systemFingerprint",
                fp.clone(),
            );
        }
        // Preserve the raw `finish_reason` when the unified enum would not
        // re-render it verbatim — e.g. `function_call` (→ `tool_calls`) or an
        // Anthropic-flavoured `end_turn` / `max_tokens` accepted here. A refusal
        // override also diverges from the wire string. Stash it under the
        // `openai` namespace so `render_response` reproduces the exact native
        // value on a same-protocol hop.
        stash_raw_finish_reason(
            &mut provider_metadata,
            PROVIDER_ID_OPENAI,
            raw_finish.as_deref(),
            finish_reason.as_ref(),
            finish_reason_str,
        );

        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details: None,
            provider_metadata,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ChatStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

/// Render one canonical [`Tool`] into a Chat Completions `tools` entry.
///
/// A [`Tool::Function`] becomes `{type:"function", function:{name, description,
/// parameters, strict?}}`; the `strict` flag is emitted when set (previously it
/// was always dropped). Returns `Some(_)`.
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-tools>
///
/// Chat Completions has **no** provider-defined ("server") tool envelope on the
/// wire — its `tools` array accepts only `{type:"function", …}`. A
/// [`Tool::ProviderDefined`] therefore has no valid Chat Completions
/// representation: emitting its source-native shape (e.g. `{type:"web_search"}`,
/// or a Codex `{type:"namespace"}` group) is a structurally-invalid `tools`
/// entry that a strict upstream (DeepSeek, Kimi, OpenAI itself) rejects — which
/// fails the **entire request** with an opaque error rather than ignoring one
/// tool. Such a server tool could not be executed on a Chat Completions upstream
/// anyway, so it is dropped (returns `None`), keeping the request — and its
/// function tools — valid. This is wire-structural, not capability gating: a
/// tool the target wire cannot express is omitted, never a function tool the
/// model might support.
fn render_chat_tool(tool: &Tool) -> Option<serde_json::Value> {
    match tool {
        Tool::Function {
            name,
            description,
            parameters,
            strict,
            ..
        } => {
            let mut function = serde_json::Map::new();
            function.insert("name".into(), name.clone().into());
            // `description` rides through as JSON null when absent for parity with
            // the prior render; callers tolerate it.
            function.insert(
                "description".into(),
                description
                    .clone()
                    .map_or(serde_json::Value::Null, serde_json::Value::String),
            );
            function.insert("parameters".into(), parameters.clone());
            if let Some(strict) = strict {
                function.insert("strict".into(), (*strict).into());
            }
            Some(serde_json::json!({ "type": "function", "function": function }))
        }
        Tool::ProviderDefined { .. } => None,
    }
}

/// Render a canonical [`ResponseFormat`] into Chat Completions' native
/// `{ type: "json_schema", json_schema: { name, description?, strict?, schema } }`.
/// OpenAI requires `name`; supply a stable default when the caller didn't set
/// one. `description` is emitted only when the canonical slot carries it (the
/// OpenAI family is the only wire that transmits a schema description).
/// <https://platform.openai.com/docs/guides/structured-outputs>
fn render_chat_response_format(rf: &ResponseFormat) -> serde_json::Value {
    let ResponseFormat::JsonSchema {
        name,
        description,
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
    if let Some(description) = description {
        js.insert("description".into(), description.clone().into());
    }
    if let Some(strict) = strict {
        js.insert("strict".into(), (*strict).into());
    }
    js.insert("schema".into(), schema.clone());
    serde_json::json!({ "type": "json_schema", "json_schema": serde_json::Value::Object(js) })
}

/// Normalize a Chat Completions `stop` value into the canonical stop-sequence
/// list. The wire accepts either a single string or an array of strings;
/// non-string array members and any other JSON shape are ignored (the canonical
/// slot is string-only, matching Anthropic's `stop_sequences` and Gemini's
/// `stopSequences`). `None`/empty yields an empty list, which renders nothing.
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-stop>
fn parse_chat_stop(value: Option<serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Promote a Chat Completions `tool_choice` into the canonical [`ToolChoice`],
/// removing it from `extra` when it maps to a known shape. Unmapped shapes
/// (e.g. `{ "type": "allowed_tools", … }`) are left untouched so they pass
/// through opaquely.
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-tool_choice>
fn parse_chat_tool_choice(extra: &mut HashMap<String, serde_json::Value>) -> Option<ToolChoice> {
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
            o.get("function")
                .and_then(|f| f.get("name"))
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

/// Render the canonical [`ToolChoice`] into Chat Completions' native shape: the
/// bare strings `auto` / `required` / `none`, or a `{ type: "function",
/// function: { name } }` object to force one tool.
fn render_chat_tool_choice(tc: &ToolChoice) -> serde_json::Value {
    match tc {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Tool { name } => serde_json::json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
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

/// Render one canonical content part into an OpenAI content-array element
/// (text + media). Reasoning / tool calls ride sibling fields, not the content
/// array, so they yield `None` here.
fn render_input_part(c: &Content) -> Option<serde_json::Value> {
    match c {
        Content::Text { text, .. } => Some(serde_json::json!({ "type": "text", "text": text })),
        Content::File {
            media_type,
            data,
            filename,
            provider_metadata,
        } => Some(if media_type.starts_with("image/") {
            // <https://platform.openai.com/docs/guides/vision>
            let mut image_url = serde_json::Map::new();
            image_url.insert("url".into(), data.to_url(media_type).into());
            // Restore the OpenAI `detail` hint from the `openai` namespace when
            // it round-tripped through `provider_metadata` (set by this
            // protocol's `parse_chat_part`).
            // <https://platform.openai.com/docs/guides/vision>
            if let Some(detail) = provider_namespace(provider_metadata, PROVIDER_ID_OPENAI)
                .and_then(|o| o.get("detail"))
            {
                image_url.insert("detail".into(), detail.clone());
            }
            serde_json::json!({
                "type": "image_url",
                "image_url": serde_json::Value::Object(image_url),
            })
        } else if let Some(format) = media_type.strip_prefix("audio/") {
            // <https://platform.openai.com/docs/guides/audio>
            let audio = match data {
                DataContent::Base64 { data } => {
                    serde_json::json!({ "data": data, "format": format })
                }
                DataContent::Url { url } => serde_json::json!({ "url": url, "format": format }),
            };
            serde_json::json!({ "type": "input_audio", "input_audio": audio })
        } else {
            // Documents (PDF, …) — OpenAI `file` part.
            let mut file = serde_json::json!({ "file_data": data.to_url(media_type) });
            if let Some(name) = filename {
                file["filename"] = serde_json::Value::String(name.clone());
            }
            serde_json::json!({ "type": "file", "file": file })
        }),
        _ => None,
    }
}

/// Render a [`ToolResultOutput`] into the value of an OpenAI tool message's
/// `content` field. A multimodal [`ToolResultOutput::Content`] becomes a
/// content-part array (reusing [`render_input_part`]); every other variant
/// collapses to a plain string (the error variants lose their flag, which the
/// OpenAI tool wire cannot represent).
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
fn render_tool_result_content(output: &ToolResultOutput) -> serde_json::Value {
    match output {
        ToolResultOutput::Content { value } => {
            let parts: Vec<serde_json::Value> = value
                .iter()
                .filter_map(tool_result_part_to_content)
                .filter_map(|c| render_input_part(&c))
                .collect();
            parts.into()
        }
        other => other.to_provider_string().into(),
    }
}

/// Lift a [`ToolResultContentPart`] into a canonical [`Content`] so the shared
/// [`render_input_part`] media renderer can be reused for tool-result content.
/// Returns `None` for a provider file reference: the OpenAI Chat Completions
/// `content` parts (`text` / `image_url` / `input_audio` / `file{file_data}`)
/// have no bare-`file_id` form, so a [`ToolResultContentPart::FileId`] has no
/// faithful representation here and is dropped rather than fabricated.
/// <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
fn tool_result_part_to_content(part: &ToolResultContentPart) -> Option<Content> {
    match part {
        ToolResultContentPart::Text { text } => Some(Content::Text {
            text: text.clone(),
            provider_metadata: ProviderMetadata::new(),
        }),
        ToolResultContentPart::Media { media_type, data } => Some(Content::File {
            media_type: media_type.clone(),
            data: data.clone(),
            filename: None,
            provider_metadata: ProviderMetadata::new(),
        }),
        ToolResultContentPart::FileId { .. } => None,
    }
}

fn render_message(m: &Message) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), role_str(m.role).into());

    if m.role == Role::Tool {
        // OpenAI tool messages carry `{tool_call_id, content}`. `content` is a
        // string, or a content-part array when the result is multimodal. The
        // wire has no error flag and no tool-name field, so an error output
        // degrades to its text/JSON string and `tool_name` is dropped.
        // <https://platform.openai.com/docs/api-reference/chat/create#chat-create-messages>
        for c in &m.content {
            if let Content::ToolResult {
                call_id, output, ..
            } = c
            {
                obj.insert("tool_call_id".into(), call_id.clone().into());
                obj.insert("content".into(), render_tool_result_content(output));
            }
        }
        return serde_json::Value::Object(obj);
    }

    // `content` is a plain string for text-only messages (back-compat) or an
    // ordered parts array when the message carries media.
    if m.content.iter().any(|c| matches!(c, Content::File { .. })) {
        let parts: Vec<serde_json::Value> =
            m.content.iter().filter_map(render_input_part).collect();
        obj.insert("content".into(), parts.into());
    } else {
        let text: String = m
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        obj.insert("content".into(), text.into());
    }

    let reasoning: String = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !reasoning.is_empty() {
        obj.insert("reasoning_content".into(), reasoning.into());
    }

    // Chat Completions has no provider-executed server-tool slot, so a
    // `provider_executed` call degrades to a plain `tool_calls` entry here (the
    // flag is dropped) — `..` ignores it deliberately.
    let tool_calls: Vec<_> = m
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
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
    /// Remembers the tool-call id per `index` (only the first chunk of a call
    /// carries `id`) so the canonical `ToolCallDelta.id` is stable across the
    /// continuation chunks.
    tool_ids: Vec<String>,
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
                            .and_then(|n| n.as_str())
                            // Some OpenAI-compatible upstreams (e.g. Kimi / DeepSeek
                            // serving stacks that emit `functions.<name>:<idx>` tool
                            // ids) re-send `"name":""` on every argument-continuation
                            // chunk. An empty string is not a name announcement, so
                            // normalize it to `None` — otherwise a downstream encoder
                            // reads it as the start of a *new* tool call.
                            .filter(|n| !n.is_empty());
                        let args = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("");
                        while self.tool_ids.len() <= idx {
                            self.tool_ids.push(String::new());
                        }
                        if let Some(id) = id {
                            self.tool_ids[idx] = id.to_string();
                        }
                        parts.push(StreamPart::ToolCallDelta {
                            id: self.tool_ids[idx].clone(),
                            name: name.map(|n| n.to_string()),
                            arguments: args.to_string(),
                        });
                    }
                }
                // Streamed web-search citations arrive on `delta.annotations`
                // (same `url_citation` shape as the non-streaming response).
                // Each becomes one whole `StreamPart::Source`; the id is
                // synthesized from the url + this chunk's annotation index.
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/chat/openai-chat-language-model.ts>
                for content in parse_chat_annotations(delta.get("annotations")) {
                    if let Content::Source { source, .. } = content {
                        parts.push(StreamPart::Source { source });
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
            StreamPart::File { .. } => {
                // Chat Completions streaming has no native file-output frame
                // (image generation is a separate API), so a generated file is
                // surfaced only on the non-streaming path. Documented limitation.
            }
            StreamPart::ServerToolCall { .. } | StreamPart::ServerToolResult { .. } => {
                // Chat Completions has no server-tool / MCP wire form (cf. the
                // provider-defined-tool drop on this wire), and emitting a
                // `tool_calls` delta would make the client try to execute a tool
                // BitRouter already ran. Router-executed tool activity is
                // therefore dropped here; the model's narration and final answer
                // still stream. Documented limitation.
            }
            StreamPart::TextStart { .. }
            | StreamPart::TextEnd { .. }
            | StreamPart::ReasoningStart { .. }
            | StreamPart::ReasoningEnd { .. } => {
                // Coarse wire: Chat Completions frames no content blocks (deltas
                // are a flat run on one `choices[0].delta`), so block-lifecycle
                // markers have no native frame and re-encode to nothing.
            }
            StreamPart::Source { source } => {
                // Re-attach a streamed citation as a `delta.annotations[]`
                // `url_citation` chunk — the location the decoder reads. Only a
                // URL source has a Chat representation; a document citation is
                // dropped here (no `url_citation` form on this wire).
                // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/chat/openai-chat-language-model.ts>
                if let Source::Url { url, title, .. } = source {
                    let mut cite = serde_json::Map::new();
                    cite.insert("url".into(), url.clone().into());
                    if let Some(title) = title {
                        cite.insert("title".into(), title.clone().into());
                    }
                    let mut delta = self.open_delta();
                    delta.insert(
                        "annotations".into(),
                        serde_json::json!([{
                            "type": "url_citation",
                            "url_citation": serde_json::Value::Object(cite),
                        }]),
                    );
                    frames.push(self.chunk(serde_json::Value::Object(delta), None));
                }
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
