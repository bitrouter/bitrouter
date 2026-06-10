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

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;

use async_trait::async_trait;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, PROVIDER_ID_ANTHROPIC, SseEvent, StreamDecoder, StreamEncoder,
    Transport, describe_deser_error, provider_defined_native,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, AuthScheme, Content, DataContent, FinishReason, GenerateResult, GenerationParams,
    Message, Prompt, ResponseFormat, Role, RoutingTarget, Source, StopDetails, StreamPart, Tool,
    ToolChoice, ToolResultContentPart, ToolResultOutput, Usage,
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
    /// Structured-output + reasoning config — `format` (`json_schema`) is
    /// promoted to the canonical `response_format` and `effort` to the canonical
    /// reasoning effort. Sibling keys pass through via `extra`.
    /// <https://platform.claude.com/docs/en/build-with-claude/structured-outputs>
    #[serde(default)]
    output_config: Option<MessagesOutputConfig>,
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `stop_sequences`, `top_k`, `metadata`,
    /// `thinking`, the deprecated flat `output_format` alias, … — rides along
    /// via `extra` and is splatted back on render. Skipped from the published
    /// schema so the documented contract is the set of typed fields;
    /// pass-through behavior is preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Messages `output_config` — structured output + reasoning effort.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesOutputConfig {
    #[serde(default)]
    format: Option<MessagesOutputFormat>,
    /// Reasoning effort (`low` | `medium` | `high` | `xhigh` | `max`).
    #[serde(default)]
    effort: Option<String>,
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Messages `output_config.format`
/// (<https://platform.claude.com/docs/en/build-with-claude/structured-outputs>).
/// Anthropic's GA structured-output format carries no name / strict.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagesOutputFormat {
    /// JSON constrained to a schema.
    JsonSchema {
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// One element of [`MessagesRequest`]'s `messages` array — a `{ role, content }` turn.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesMessage {
    role: String,
    /// String or an array of content blocks.
    content: serde_json::Value,
}

/// One element of [`MessagesRequest`]'s `tools` array.
///
/// Anthropic uses one array for both kinds of tool:
/// - a **client (function) tool** — `{name, description?, input_schema}` (no `type`);
/// - a **provider-defined (server) tool** — `{type:"web_search_20250305"|…, name, …config}`
///   (web search, code execution, computer use, bash, text editor), where `type`
///   is the dated tool version and `name` is the stable tool name.
///
/// A versioned `type` discriminates a server tool; its config keys ride in
/// `extra` so they are preserved verbatim into `Tool::ProviderDefined.args`.
/// <https://docs.claude.com/en/docs/agents-and-tools/tool-use/overview>
/// <https://docs.claude.com/en/docs/agents-and-tools/tool-use/web-search-tool>
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesTool {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: serde_json::Value,
    /// Server-tool configuration keys (e.g. `max_uses`, `allowed_domains`,
    /// `display_width_px`) for a provider-defined tool. Preserved verbatim.
    /// Skipped from the published schema — the documented contract is the typed
    /// client-tool shape.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: HashMap<String, serde_json::Value>,
}

/// Parse one Anthropic `tools` entry into a canonical [`Tool`]. A versioned
/// `type` marks a provider-defined server tool (namespaced `anthropic.<type>`,
/// config keys preserved verbatim as `args`); a typeless entry is a client
/// function tool (`{name, description?, input_schema}`). An entry with neither a
/// `type` nor a `name` is dropped — there is nothing to forward.
fn parse_messages_tool(t: MessagesTool) -> Option<Tool> {
    if let Some(kind) = t.kind {
        // Server tool: `{type:"web_search_20250305", name:"web_search", …config}`.
        return Some(Tool::ProviderDefined {
            id: format!("{PROVIDER_ID_ANTHROPIC}.{kind}"),
            name: t.name.unwrap_or_else(|| kind.clone()),
            args: serde_json::Value::Object(t.extra.into_iter().collect()),
        });
    }
    Some(Tool::Function {
        name: t.name?,
        description: t.description,
        parameters: t.input_schema,
        // Anthropic client tools carry no `strict` slot.
        strict: None,
    })
}

/// Render one canonical [`Tool`] into an Anthropic `tools` entry.
///
/// A [`Tool::Function`] becomes `{name, description?, input_schema}`. Anthropic
/// has **no** `strict` slot, so [`Tool::Function::strict`] is intentionally
/// dropped here (documented; the same drop applies to structured-output
/// `strict`). A [`Tool::ProviderDefined`] renders to its source-native shape via
/// [`provider_defined_native`]: an `anthropic.*` id reproduces the exact server
/// tool (`{type:<version>, name, …args}`) for a lossless same-protocol
/// round-trip; a foreign-provider id is preserved verbatim (faithful
/// passthrough) so the upstream decides.
/// <https://docs.claude.com/en/docs/agents-and-tools/tool-use/overview>
fn render_messages_tool(tool: &Tool) -> serde_json::Value {
    match tool {
        Tool::Function {
            name,
            description,
            parameters,
            strict: _,
        } => {
            serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            })
        }
        Tool::ProviderDefined { id, name, args } => provider_defined_native(id, name, args),
    }
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

/// Parse an Anthropic image/document `source` object into a canonical
/// `(media_type, DataContent)`. A `base64` source carries an explicit
/// `media_type`; a `url` source carries none, so the caller's fallback (derived
/// from the block kind) is used.
/// <https://docs.anthropic.com/en/docs/build-with-claude/vision>
fn parse_anthropic_source(
    source: Option<&serde_json::Value>,
    media_type_fallback: &str,
) -> (String, DataContent) {
    let media_type = source
        .and_then(|s| s.get("media_type"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| media_type_fallback.to_string());
    let data = if source.and_then(|s| s.get("type")).and_then(|t| t.as_str()) == Some("url") {
        DataContent::Url {
            url: source
                .and_then(|s| s.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string(),
        }
    } else {
        DataContent::Base64 {
            data: source
                .and_then(|s| s.get("data"))
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
        }
    };
    (media_type, data)
}

/// Parse the `content` of an Anthropic `tool_result` block into a canonical
/// [`ToolResultOutput`], honoring the block's `is_error` flag.
///
/// `is_error` selects the error variants: a string error → [`ToolResultOutput::ErrorText`],
/// a structured error → [`ToolResultOutput::ErrorJson`]. Without the flag, a
/// string → [`ToolResultOutput::Text`], a block array carrying media →
/// [`ToolResultOutput::Content`] (text + image parts in order), a text-only
/// block array collapses to `Text`, and any other JSON value →
/// [`ToolResultOutput::Json`].
/// <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
fn parse_tool_result_output(
    content: Option<&serde_json::Value>,
    is_error: bool,
) -> ToolResultOutput {
    let Some(value) = content else {
        // An absent body still carries the error flag faithfully.
        return if is_error {
            ToolResultOutput::ErrorText {
                value: String::new(),
            }
        } else {
            ToolResultOutput::Text {
                value: String::new(),
            }
        };
    };
    if is_error {
        return match value {
            serde_json::Value::String(s) => ToolResultOutput::ErrorText { value: s.clone() },
            serde_json::Value::Array(_) => ToolResultOutput::ErrorText {
                value: tool_result_text(value),
            },
            other => ToolResultOutput::ErrorJson {
                value: other.clone(),
            },
        };
    }
    match value {
        serde_json::Value::String(s) => ToolResultOutput::Text { value: s.clone() },
        serde_json::Value::Array(blocks) => {
            let parts = parse_tool_result_blocks(blocks);
            let has_media = parts
                .iter()
                .any(|p| matches!(p, ToolResultContentPart::Media { .. }));
            if has_media {
                ToolResultOutput::Content { value: parts }
            } else {
                ToolResultOutput::Text {
                    value: tool_result_text(value),
                }
            }
        }
        other => ToolResultOutput::Json {
            value: other.clone(),
        },
    }
}

/// Parse an Anthropic `tool_result` content-block array into ordered
/// [`ToolResultContentPart`]s. `text` blocks become text parts; `image` blocks
/// become media parts. Unknown block kinds are skipped (forward compatibility).
/// <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
fn parse_tool_result_blocks(blocks: &[serde_json::Value]) -> Vec<ToolResultContentPart> {
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => Some(ToolResultContentPart::Text {
                text: b
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }),
            Some("image") => {
                let (media_type, data) = parse_anthropic_source(b.get("source"), "image/");
                Some(ToolResultContentPart::Media { media_type, data })
            }
            _ => None,
        })
        .collect()
}

/// Flatten the text of an Anthropic `tool_result` content value (string, or a
/// block array) — used as the lossless string fallback for providers that
/// cannot carry structure.
/// <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
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
                    // A client `tool_use` block, or a provider-executed
                    // `server_tool_use` block (web search, code execution, …).
                    // They share the same `{id, name, input}` shape and differ
                    // only by the block `type`; the latter sets
                    // `provider_executed` so it is not re-sent as a client call.
                    // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
                    "tool_use" | "server_tool_use" => out.push(Content::ToolCall {
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
                        provider_executed: block_type == "server_tool_use",
                    }),
                    // Anthropic `tool_result` block `{tool_use_id, content, is_error}`:
                    // `content` is a string or a block array (text / image); the
                    // optional `is_error` flag promotes the output to an error
                    // variant. The wire carries no tool name → `tool_name: None`.
                    // <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
                    "tool_result" => out.push(Content::ToolResult {
                        call_id: block
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_name: None,
                        output: parse_tool_result_output(
                            block.get("content"),
                            block
                                .get("is_error")
                                .and_then(|e| e.as_bool())
                                .unwrap_or(false),
                        ),
                    }),
                    // image/* and documents (PDF, …) -> a canonical File part.
                    // <https://docs.anthropic.com/en/docs/build-with-claude/vision>
                    "image" | "document" => {
                        // A `url` source carries no media type; derive a prefix
                        // from the block kind so modality detection still works.
                        let fallback = if block_type == "image" {
                            "image/"
                        } else {
                            "application/"
                        };
                        let (media_type, data) =
                            parse_anthropic_source(block.get("source"), fallback);
                        out.push(Content::File {
                            media_type,
                            data,
                            filename: None,
                            extra: Default::default(),
                        });
                    }
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
        // Mid-conversation system messages: Opus 4.8 accepts `role: "system"`
        // entries at non-first positions in `messages` (GA, no beta header), so
        // operator instructions can change mid-session without invalidating the
        // prompt cache. Map them to the canonical System role and let the
        // upstream model decide whether it supports them.
        // <https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages>
        "system" => Ok(Role::System),
        // Tool results ride inside a user-role message; any other role is a
        // hard error (#454-4).
        other => Err(BitrouterError::bad_request(format!(
            "unknown anthropic message role '{other}' (expected user/assistant/system)"
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

/// Parse Anthropic's `stop_details` object (present on refusals) into the
/// canonical [`StopDetails`]. Returns `None` when absent or when it carries
/// neither a category nor an explanation.
/// <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
fn parse_stop_details(value: &serde_json::Value) -> Option<StopDetails> {
    let category = value
        .get("category")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let explanation = value
        .get("explanation")
        .and_then(|e| e.as_str())
        .map(str::to_string);
    if category.is_none() && explanation.is_none() {
        return None;
    }
    Some(StopDetails {
        category,
        explanation,
    })
}

/// Render a canonical [`StopDetails`] back into Anthropic's `stop_details`
/// object. It accompanies a refusal, so `type` is fixed to `"refusal"`.
/// <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
fn render_stop_details(details: &StopDetails) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), "refusal".into());
    if let Some(category) = &details.category {
        obj.insert("category".into(), category.clone().into());
    }
    if let Some(explanation) = &details.explanation {
        obj.insert("explanation".into(), explanation.clone().into());
    }
    serde_json::Value::Object(obj)
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

        // Anthropic mixes client (function) tools and provider-defined (server)
        // tools in one array. A versioned `type` (`web_search_20250305`,
        // `code_execution_20250522`, `computer_20250124`, `bash_20250124`,
        // `text_editor_20250124`, …) marks a server tool — namespaced
        // `anthropic.<type>`, with its config keys preserved verbatim as `args`.
        // A typeless entry is a client tool (`{name, description?, input_schema}`).
        // <https://docs.claude.com/en/docs/agents-and-tools/tool-use/overview>
        let tools = req
            .tools
            .into_iter()
            .filter_map(parse_messages_tool)
            .collect();

        // Messages' GA structured outputs are typed under `output_config`
        // (`format` = json_schema → canonical; `effort` → canonical reasoning
        // effort so it round-trips across protocols, mirroring Chat Completions'
        // `reasoning_effort` and Responses' `reasoning.effort`). Unknown
        // `output_config` siblings and the deprecated flat `output_format` alias
        // (vercel/ai#12298) still ride through `extra`.
        let mut extra = req.extra;
        let mut reasoning_effort = None;
        let mut response_format = None;
        if let Some(oc) = req.output_config {
            let MessagesOutputConfig {
                format,
                effort,
                extra: oc_extra,
            } = oc;
            reasoning_effort = effort;
            if let Some(MessagesOutputFormat::JsonSchema { schema }) = format {
                // Anthropic's GA format carries no name / strict.
                response_format = Some(ResponseFormat::JsonSchema {
                    name: None,
                    strict: None,
                    schema,
                });
            }
            // Preserve unknown `output_config` siblings for render passthrough.
            if !oc_extra.is_empty() {
                extra.insert(
                    "output_config".to_string(),
                    serde_json::Value::Object(oc_extra.into_iter().collect()),
                );
            }
        }
        // Deprecated flat `output_format` alias — only when `output_config` did
        // not already supply a structured-output format.
        if response_format.is_none()
            && let Some(rf) = parse_legacy_output_format(&extra)
        {
            extra.remove("output_format");
            response_format = Some(rf);
        }

        // Promote `tool_choice` into the typed slot and drop it from `extra` so it
        // is not also splatted back verbatim — otherwise an Anthropic-shaped
        // choice would leak into a non-Anthropic upstream.
        let tool_choice = extra.remove("tool_choice").map(parse_messages_tool_choice);

        Ok(Prompt {
            model: req.model,
            system,
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_tokens,
                reasoning_effort,
                response_modalities: Vec::new(),
                tool_choice,
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
        let mut content: Vec<serde_json::Value> = result
            .content
            .iter()
            .filter_map(render_content_block)
            .collect();
        // Re-attach web-search citations as a valid `server_tool_use` ↔
        // `web_search_tool_result` pair (the faithful Anthropic citation
        // location — see `render_web_search_result_blocks`). Appended after the
        // answer blocks, matching the wire order (the result follows the text it
        // cites). When the originating provider-executed call is already present
        // among the answer blocks, only the result block is appended and it
        // reuses that call's id; otherwise a synthetic call block is prepended.
        content.extend(render_web_search_result_blocks(result));
        let usage = result.usage.unwrap_or_default();
        let stop_reason = result
            .finish_reason
            .as_ref()
            .map(finish_to_stop_reason)
            .map_or(serde_json::Value::Null, serde_json::Value::String);
        let mut body = serde_json::Map::new();
        body.insert("id".into(), request_id.into());
        body.insert("type".into(), "message".into());
        body.insert("role".into(), "assistant".into());
        body.insert("model".into(), prompt.model.clone().into());
        body.insert("content".into(), serde_json::Value::Array(content));
        body.insert("stop_reason".into(), stop_reason);
        body.insert("usage".into(), render_usage(&usage));
        // Echo a refusal's structured detail back in Anthropic's shape so a
        // Messages client sees the same `stop_details` the upstream produced.
        // <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
        if let Some(details) = &result.stop_details {
            body.insert("stop_details".into(), render_stop_details(details));
        }
        Ok(serde_json::Value::Object(body))
    }

    fn stream_encoder(&self, request_id: &str, model: &str) -> Box<dyn StreamEncoder> {
        Box::new(MessagesStreamEncoder {
            request_id: request_id.to_string(),
            model: model.to_string(),
            started: false,
            block_open: false,
            block_kind: None,
            block_index: 0,
            pending_sources: Vec::new(),
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
                    .map(render_messages_tool)
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
        // Anthropic groups structured-outputs (`format`) and the reasoning
        // `effort` knob under one `output_config` object. Seed it from any
        // pass-through `output_config` so unknown siblings the inbound adapter
        // left in `extra` survive, then layer the canonical `response_format`
        // and `reasoning_effort` on top so cross-protocol routing carries both
        // (e.g. a Chat Completions client's `reasoning_effort` reaches an
        // Anthropic upstream as `output_config.effort`). The canonical format's
        // `name` / `strict` are intentionally dropped — Anthropic's
        // schema-constrained sampling has no concept of either.
        // <https://platform.claude.com/docs/en/build-with-claude/effort>
        let mut output_config = prompt
            .params
            .extra
            .get("output_config")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(rf) = &prompt.response_format {
            output_config.insert("format".into(), render_messages_response_format(rf));
        }
        if let Some(effort) = &prompt.params.reasoning_effort {
            output_config.insert("effort".into(), effort.clone().into());
        }
        if !output_config.is_empty() {
            req.insert(
                "output_config".into(),
                serde_json::Value::Object(output_config),
            );
        }
        // Render the canonical tool_choice into Anthropic's native shape, before
        // the extras splat so the typed slot wins over any stale `tool_choice`.
        if let Some(tc) = &prompt.params.tool_choice {
            req.insert("tool_choice".into(), render_messages_tool_choice(tc));
        }
        // Splat anthropic-specific extras (stop_sequences, top_k, …) back into
        // the outbound request. Typed fields win over same-named extras.
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
        // Running citation counter so synthesized source ids stay unique across
        // every text block / web_search_tool_result block in the reply.
        let mut source_index = 0usize;
        for block in content_blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    content.push(Content::Text {
                        text: block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    });
                    // A text block may carry inline web-search citations; lift
                    // them into `Content::Source` parts right after the text.
                    content.extend(parse_messages_block_sources(block, &mut source_index));
                }
                Some("thinking") | Some("redacted_thinking") => content.push(Content::Reasoning {
                    text: block
                        .get("thinking")
                        .or_else(|| block.get("data"))
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }),
                // A client `tool_use` block, or a provider-executed
                // `server_tool_use` block. Anthropic runs server tools (e.g.
                // `web_search`) itself and emits a `server_tool_use` block with
                // an `srvtoolu_…` id followed by a `*_tool_result` block; mark
                // it `provider_executed` so a follow-up turn does not re-issue it
                // as a client call. The paired result block is consumed
                // separately and is not a tool *call*.
                // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
                Some(kind @ ("tool_use" | "server_tool_use")) => content.push(Content::ToolCall {
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
                    provider_executed: kind == "server_tool_use",
                }),
                // A `web_search_tool_result` block carries the raw search hits
                // (`content[]` of `web_search_result`). Lift each into a
                // `Content::Source`; the paired `server_tool_use` call block was
                // already captured above as a provider-executed tool call.
                // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
                Some("web_search_tool_result") => {
                    content.extend(parse_messages_block_sources(block, &mut source_index));
                }
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
        // On a refusal, Opus 4.7+ returns a `stop_details` object with the
        // policy `category` (`cyber` | `bio` | null) and an `explanation`.
        // Surface it so callers can route refusal classes; it is null for every
        // non-refusal stop reason.
        // <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
        let stop_details = body.get("stop_details").and_then(parse_stop_details);
        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(MessagesStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
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

/// Parse Anthropic's `tool_choice` object into the canonical [`ToolChoice`].
/// Anthropic uses `{type:"auto"|"any"|"tool"|"none", name?}`: `any` is "must
/// call some tool" (canonical `Required`), `tool` names a specific tool. An
/// unrecognised shape is preserved verbatim via [`ToolChoice::Other`].
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/implement-tool-use#controlling-claudes-output>
fn parse_messages_tool_choice(value: serde_json::Value) -> ToolChoice {
    match value.get("type").and_then(|t| t.as_str()) {
        Some("auto") => ToolChoice::Auto,
        Some("none") => ToolChoice::None,
        Some("any") => ToolChoice::Required,
        Some("tool") => match value.get("name").and_then(|n| n.as_str()) {
            Some(name) => ToolChoice::Tool {
                name: name.to_string(),
            },
            None => ToolChoice::Other { value },
        },
        _ => ToolChoice::Other { value },
    }
}

/// Render a canonical [`ToolChoice`] into Anthropic's native `tool_choice`
/// object. `Required` maps to `{type:"any"}`; `Tool` to `{type:"tool",name}`.
/// Anthropic natively supports `{type:"none"}`, so `None` round-trips faithfully
/// (rather than the AI SDK's drop-the-tools workaround).
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/implement-tool-use#controlling-claudes-output>
fn render_messages_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Auto => serde_json::json!({ "type": "auto" }),
        ToolChoice::None => serde_json::json!({ "type": "none" }),
        ToolChoice::Required => serde_json::json!({ "type": "any" }),
        ToolChoice::Tool { name } => serde_json::json!({ "type": "tool", "name": name }),
        ToolChoice::Other { value } => value.clone(),
    }
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
        let headers = request.headers_mut();
        // Exactly one credential header — never both. The Anthropic API
        // rejects a request carrying `x-api-key` and `Authorization` together,
        // so the scheme is chosen per target (`RoutingTarget::auth_scheme`).
        match target.auth_scheme {
            AuthScheme::XApiKey => {
                let value = reqwest::header::HeaderValue::from_str(key).map_err(|e| {
                    BitrouterError::internal(format!("invalid api key for x-api-key header: {e}"))
                })?;
                headers.insert("x-api-key", value);
            }
            AuthScheme::Bearer => {
                let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|e| {
                        BitrouterError::internal(format!(
                            "invalid api key for authorization header: {e}"
                        ))
                    })?;
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        Ok(request)
    }
}

/// Lift Anthropic web-search citations out of a response `content` block into
/// canonical [`Content::Source`] (URL) parts, with `next_index` tracking the
/// running citation count so synthesized ids stay unique across blocks.
///
/// Two block shapes carry web-search citations:
/// - a `text` block whose `citations[]` array holds
///   `{type:"web_search_result_location", url, title, cited_text, encrypted_index}`
///   entries (an inline citation linked to a span of the answer text), and
/// - a `web_search_tool_result` block whose `content[]` array holds
///   `{type:"web_search_result", url, title, page_age, ...}` entries (the raw
///   search hits).
///
/// Both map to `Source::Url{url, title}`. The wire carries no citation id, so
/// one is synthesized from the url + running index. The inline `cited_text` and
/// the `encrypted_index`/`page_age` provider fields have no slot on the
/// canonical `Source` and are dropped — see [`Source`] (the text-to-source
/// linkage loss is inherent to the V3 parity target).
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
fn parse_messages_block_sources(block: &serde_json::Value, next_index: &mut usize) -> Vec<Content> {
    let entries = match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => block.get("citations").and_then(|c| c.as_array()),
        Some("web_search_tool_result") => block.get("content").and_then(|c| c.as_array()),
        _ => None,
    };
    let Some(entries) = entries else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        // A text block's `citations[]` may also carry non-web citation kinds
        // (`page_location` / `char_location` for document citations); only the
        // URL web-search kind has a faithful `Source::Url` form today.
        let kind = entry.get("type").and_then(|t| t.as_str());
        if !matches!(
            kind,
            Some("web_search_result_location") | Some("web_search_result")
        ) {
            continue;
        }
        let Some(url) = entry.get("url").and_then(|u| u.as_str()) else {
            continue;
        };
        let title = entry
            .get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        out.push(Content::Source {
            source: Source::Url {
                id: Source::synthesize_id(url, *next_index),
                url: url.to_string(),
                title,
            },
        });
        *next_index += 1;
    }
    out
}

/// Synthetic `server_tool_use` id used to pair a synthesized web-search
/// call with its `web_search_tool_result` when the canonical content has no
/// originating provider-executed call to borrow a real id from (cross-protocol,
/// e.g. a Gemini grounding response rendered to an Anthropic client). It is
/// opaque to clients and only correlates the two blocks on the wire.
const SYNTHETIC_WEB_SEARCH_ID: &str = "srvtoolu_citations";

/// Anthropic's server `web_search` tool name, used on a synthesized
/// `server_tool_use` block so the emitted pair is a valid web-search call.
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
const ANTHROPIC_WEB_SEARCH_TOOL: &str = "web_search";

/// True when a canonical [`Content::ToolCall`] is a provider-executed web-search
/// call — the originating block of a `web_search_tool_result`. Anthropic's
/// server web search emits a `server_tool_use{name:"web_search"}` block paired
/// with its result by a shared `tool_use_id`; on parse that block becomes a
/// `ToolCall{provider_executed:true, name:"web_search"}`, so the render path
/// recognizes it here to reuse its real id when re-pairing.
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
fn is_web_search_call(c: &Content) -> bool {
    matches!(
        c,
        Content::ToolCall {
            provider_executed: true,
            name,
            ..
        } if name == ANTHROPIC_WEB_SEARCH_TOOL
    )
}

/// Render canonical [`Content::Source`] parts into a valid Anthropic
/// `server_tool_use` ↔ `web_search_tool_result` pair. The result block's
/// `content[]` carries one `web_search_result` entry per URL source.
///
/// A `web_search_tool_result` block is the faithful render location among the
/// two Anthropic citation shapes: it natively holds an array of `{url, title}`
/// results with **no** required text linkage, so it reproduces url+title+id
/// losslessly. The alternative — `citations[]` on a `text` block — would demand
/// a `cited_text` span and char offsets the canonical `Source` cannot
/// reconstruct, fabricating the very text-linkage data the parity target drops.
/// [`Source::Document`] citations have no `web_search_result` form and are
/// skipped here. Returns an empty Vec when the result carries no URL sources.
///
/// **Pairing strategy (correlate by order).** On the Anthropic wire a
/// `web_search_tool_result` is invalid on its own: it must pair with a
/// `server_tool_use` block sharing one `tool_use_id`, or a client echoing the
/// assistant turn into a follow-up triggers `invalid_request_error`.
/// - Same-protocol: the upstream `server_tool_use` parsed to a
///   `ToolCall{provider_executed:true, name:"web_search"}` and is re-rendered as
///   a real `server_tool_use` by [`render_content_block`]. Here we detect that
///   preceding call and **reuse its id** as `tool_use_id` (real pairing; no
///   second `server_tool_use` is emitted), so `srvtoolu_…` ids correlate.
/// - Cross-protocol (no such call, e.g. Gemini grounding → Anthropic client):
///   we **synthesize** a matching `server_tool_use{id, name:"web_search",
///   input:{}}` immediately before the result block so the pair is still valid.
///
/// Preserving the *exact* original id across a full canonical round-trip is
/// deferred to provider-metadata plumbing; the emitted wire must never be
/// invalid in the meantime.
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
fn render_web_search_result_blocks(result: &GenerateResult) -> Vec<serde_json::Value> {
    let results: Vec<serde_json::Value> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Source {
                source: Source::Url { url, title, .. },
            } => {
                let mut entry = serde_json::Map::new();
                entry.insert("type".into(), "web_search_result".into());
                entry.insert("url".into(), url.clone().into());
                if let Some(title) = title {
                    entry.insert("title".into(), title.clone().into());
                }
                Some(serde_json::Value::Object(entry))
            }
            _ => None,
        })
        .collect();
    if results.is_empty() {
        return Vec::new();
    }
    // Correlate by order: borrow the id of the originating provider-executed
    // `web_search` call. All URL sources collapse into one result block, and
    // Anthropic emits one web-search call per result block, so the last such
    // call in the content is the originator. That call is already re-rendered as
    // a real `server_tool_use` by `render_content_block`, so reusing its id
    // keeps the pair correlated without emitting a duplicate call block — and
    // crucially without leaving that real call orphaned (the bug the synthetic
    // `srvtoolu_citations` placeholder used to cause). Scanning the whole
    // content (not just before the first source) covers the wire shape where an
    // inline-cited text block's `Source` precedes the `server_tool_use`.
    let paired_id = result.content.iter().rev().find_map(|c| match c {
        Content::ToolCall { id, .. } if is_web_search_call(c) => Some(id.clone()),
        _ => None,
    });
    match paired_id {
        // Real pairing: the originating `server_tool_use` already rendered with
        // this id; only the result block is added, sharing that id.
        Some(id) => vec![serde_json::json!({
            "type": "web_search_tool_result",
            "tool_use_id": id,
            "content": results,
        })],
        // No originating call (cross-protocol): synthesize the `server_tool_use`
        // so the emitted pair is valid, both blocks sharing one synthetic id.
        None => vec![
            serde_json::json!({
                "type": "server_tool_use",
                "id": SYNTHETIC_WEB_SEARCH_ID,
                "name": ANTHROPIC_WEB_SEARCH_TOOL,
                "input": {},
            }),
            serde_json::json!({
                "type": "web_search_tool_result",
                "tool_use_id": SYNTHETIC_WEB_SEARCH_ID,
                "content": results,
            }),
        ],
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
            provider_executed,
        } => {
            let input: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
            // Reproduce the server-tool block shape on the same wire: a
            // provider-executed call (e.g. web search) renders as
            // `server_tool_use`, a client call as `tool_use`. Both carry the
            // identical `{id, name, input}` payload — only the block type
            // differs.
            // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
            let block_type = if *provider_executed {
                "server_tool_use"
            } else {
                "tool_use"
            };
            Some(serde_json::json!({
                "type": block_type, "id": id, "name": name, "input": input,
            }))
        }
        // tool results are request-side only; not part of an assistant reply
        Content::ToolResult { .. } => None,
        // image/* -> an `image` block, everything else -> a `document` block.
        // Source is `{type:base64,media_type,data}` or `{type:url,url}`.
        // <https://docs.anthropic.com/en/docs/build-with-claude/vision>
        Content::File {
            media_type, data, ..
        } => {
            let source = match data {
                DataContent::Base64 { data } => serde_json::json!({
                    "type": "base64", "media_type": media_type, "data": data
                }),
                // Anthropic's `url` source carries no media_type, so an
                // `image/png` + Url loses its subtype on a round-trip — the kind
                // (image vs document) is recovered from the block type on parse,
                // so modality detection still survives.
                DataContent::Url { url } => serde_json::json!({ "type": "url", "url": url }),
            };
            let block_type = if media_type.starts_with("image/") {
                "image"
            } else {
                "document"
            };
            Some(serde_json::json!({ "type": block_type, "source": source }))
        }
        // Citations are not a per-block render: they are collected across the
        // whole reply and re-attached as a `server_tool_use` ↔
        // `web_search_tool_result` pair by `render_response` (see
        // `render_web_search_result_blocks`). Skip here.
        Content::Source { .. } => None,
    }
}

/// Render one canonical [`ToolResultOutput`] into an Anthropic `tool_result`
/// block body: `(content, is_error)`. A multimodal [`ToolResultOutput::Content`]
/// becomes a block array (`text` + `image` blocks); every other variant becomes
/// a string. The error variants set `is_error: true`; `ErrorJson` is stringified
/// because the Anthropic wire's error body is a string, not structured JSON.
/// <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
fn render_tool_result_content(output: &ToolResultOutput) -> (serde_json::Value, bool) {
    let is_error = output.is_error();
    let content = match output {
        ToolResultOutput::Content { value } => {
            let blocks: Vec<serde_json::Value> = value
                .iter()
                .filter_map(|p| match p {
                    ToolResultContentPart::Text { text } => {
                        Some(serde_json::json!({ "type": "text", "text": text }))
                    }
                    // Anthropic `tool_result` content accepts only `text` and
                    // `image` blocks — there is no `document`/audio block inside a
                    // tool result. Emit an `image` block only for `image/*` media;
                    // skip any other media type rather than mislabeling, say, a
                    // PDF or audio clip as an image (which the API would reject or
                    // misinterpret).
                    // <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
                    ToolResultContentPart::Media { media_type, data }
                        if media_type.starts_with("image/") =>
                    {
                        let source = match data {
                            DataContent::Base64 { data } => serde_json::json!({
                                "type": "base64", "media_type": media_type, "data": data
                            }),
                            DataContent::Url { url } => {
                                serde_json::json!({ "type": "url", "url": url })
                            }
                        };
                        Some(serde_json::json!({ "type": "image", "source": source }))
                    }
                    // Non-image media and provider file references have no
                    // tool_result block on the Anthropic wire; drop them.
                    ToolResultContentPart::Media { .. } | ToolResultContentPart::FileId { .. } => {
                        None
                    }
                })
                .collect();
            serde_json::Value::Array(blocks)
        }
        other => serde_json::Value::String(other.to_provider_string()),
    };
    (content, is_error)
}

fn render_message(m: &Message) -> serde_json::Value {
    // Canonical Tool-role messages become Anthropic user messages carrying
    // tool_result blocks.
    // <https://docs.anthropic.com/en/docs/build-with-claude/tool-use#tool-result>
    if m.role == Role::Tool {
        let blocks: Vec<serde_json::Value> = m
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolResult {
                    call_id, output, ..
                } => {
                    let (content, is_error) = render_tool_result_content(output);
                    let mut block = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": content,
                    });
                    // Only emit `is_error` when set — Anthropic treats its absence
                    // as `false`, so omitting it keeps non-error results clean.
                    if is_error {
                        block["is_error"] = serde_json::Value::Bool(true);
                    }
                    Some(block)
                }
                _ => None,
            })
            .collect();
        return serde_json::json!({ "role": "user", "content": blocks });
    }

    let role = match m.role {
        Role::Assistant => "assistant",
        // Mid-conversation system messages render as `role: "system"` entries so
        // the request is serialized faithfully; the upstream model (Opus 4.8+)
        // decides whether to honor them. Top-level system still rides the
        // out-of-band `system` field set in `render_request`.
        // <https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages>
        Role::System => "system",
        Role::User => "user",
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
                // A streamed `web_search_tool_result` block carries its full
                // `content[]` result array up-front on the start frame (it is
                // not chunked), so lift each hit into a whole `StreamPart::Source`
                // here — no buffering needed. The companion `citations_delta`
                // path (inline citations linked to text spans) is **not** wired:
                // it would require buffering citations against an open text block
                // and carries the `cited_text` char-range linkage the canonical
                // `Source` drops anyway — documented gap, parsed only on the
                // non-streaming path.
                // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
                if let Some(block) = block
                    && block.get("type").and_then(|t| t.as_str()) == Some("web_search_tool_result")
                {
                    let mut idx = 0usize;
                    for content in parse_messages_block_sources(block, &mut idx) {
                        if let Content::Source { source } = content {
                            parts.push(StreamPart::Source { source });
                        }
                    }
                    return Ok(parts);
                }
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
    /// `web_search_result` entries buffered from streamed `StreamPart::Source`
    /// parts, flushed as one collapsed `server_tool_use` ↔
    /// `web_search_tool_result` pair (see `flush_pending_sources`). Buffered
    /// rather than emitted per-source so the streamed wire matches the
    /// non-streaming render (one block) and stays a valid pair.
    pending_sources: Vec<serde_json::Value>,
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

    /// Flush any buffered streamed citations as one collapsed
    /// `server_tool_use` ↔ `web_search_tool_result` pair. `StreamPart` carries
    /// no provider-executed flag, so the stream never reconstructs the
    /// originating call — the pair is always synthesized (both blocks sharing
    /// one synthetic `tool_use_id`) so a client echoing the turn into a
    /// follow-up request does not see an orphan result block
    /// (`invalid_request_error`). Each block opens and closes as a whole
    /// `content_block`, mirroring the non-streaming render
    /// (`render_web_search_result_blocks`).
    /// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
    fn flush_pending_sources(&mut self, frames: &mut Vec<SseFrame>) {
        if self.pending_sources.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.pending_sources);
        self.close_block(frames);
        // Synthesized originating call, so the result block is a valid pair.
        frames.push(Self::ev(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": {
                    "type": "server_tool_use",
                    "id": SYNTHETIC_WEB_SEARCH_ID,
                    "name": ANTHROPIC_WEB_SEARCH_TOOL,
                    "input": {},
                },
            }),
        ));
        frames.push(Self::ev(
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": self.block_index }),
        ));
        self.block_index += 1;
        frames.push(Self::ev(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": SYNTHETIC_WEB_SEARCH_ID,
                    "content": results,
                },
            }),
        ));
        frames.push(Self::ev(
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": self.block_index }),
        ));
        self.block_index += 1;
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
        // Collapse all buffered citations into one pair just before the
        // terminal frames, matching the non-streaming render's tail placement.
        self.flush_pending_sources(frames);
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
            StreamPart::File { .. } => {
                // Anthropic Messages streaming has no file-output content block;
                // a generated file is surfaced only on the non-streaming path.
            }
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
            StreamPart::Source { source } => {
                // Buffer a streamed citation as a `web_search_result` entry; all
                // entries flush together as one collapsed `server_tool_use` ↔
                // `web_search_tool_result` pair at terminal
                // (`flush_pending_sources`), matching the non-streaming render's
                // single-block shape. Only a URL source has a `web_search_result`
                // form; a document citation is dropped here (no such block on
                // this wire).
                // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
                if let Source::Url { url, title, .. } = source {
                    let mut entry = serde_json::Map::new();
                    entry.insert("type".into(), "web_search_result".into());
                    entry.insert("url".into(), url.clone().into());
                    if let Some(title) = title {
                        entry.insert("title".into(), title.clone().into());
                    }
                    self.pending_sources.push(serde_json::Value::Object(entry));
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
