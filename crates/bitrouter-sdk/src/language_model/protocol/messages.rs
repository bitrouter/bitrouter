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
    Transport, describe_deser_error, provider_defined_native, rendered_finish_reason,
    stash_raw_finish_reason,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, AuthScheme, Content, DataContent, FinishReason, GenerateResult, GenerationParams,
    Message, Prompt, ProviderMetadata, ResponseFormat, Role, RoutingTarget, Source, StopDetails,
    StreamPart, Tool, ToolChoice, ToolResultContentPart, ToolResultOutput, Usage,
    provider_namespace, set_provider_metadata,
};

/// The metadata key, within the `anthropic` namespace, under which a block /
/// tool / message / system `cache_control` object rides. Matches the Vercel AI
/// SDK's `providerOptions.anthropic.cacheControl` naming.
/// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
const ANTHROPIC_CACHE_CONTROL: &str = "cacheControl";
/// The metadata key carrying an Anthropic thinking block's `signature` — the
/// opaque token that lets a thinking block be replayed on a follow-up turn.
/// <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
const ANTHROPIC_SIGNATURE: &str = "signature";
/// The metadata key marking a `redacted_thinking` block, whose encrypted body
/// rides under [`ANTHROPIC_REDACTED_DATA`]. A bare boolean `true`.
const ANTHROPIC_REDACTED: &str = "redactedThinking";
/// The metadata key carrying a `redacted_thinking` block's encrypted `data`
/// payload, so the block round-trips byte-for-byte.
const ANTHROPIC_REDACTED_DATA: &str = "redactedData";
/// The metadata key carrying the originating `server_tool_use` `tool_use_id`
/// that a `web_search_tool_result` block paired with, so the render path can
/// restore the exact pairing id instead of reusing one by position.
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
const ANTHROPIC_TOOL_USE_ID: &str = "toolUseId";
/// The metadata key tagging a [`Content::ToolCall`] / [`Content::ToolResult`] as
/// an Anthropic **MCP** block. Set to the marker [`ANTHROPIC_MCP_TOOL_USE`] so
/// the render path re-emits `mcp_tool_use` / `mcp_tool_result` rather than a
/// plain `tool_use` / `tool_result`, mirroring the AI SDK's
/// `providerOptions.anthropic.type === 'mcp-tool-use'` gate.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
const ANTHROPIC_MCP_TYPE: &str = "type";
/// The [`ANTHROPIC_MCP_TYPE`] marker value identifying an MCP tool-use block.
const ANTHROPIC_MCP_TOOL_USE: &str = "mcp-tool-use";
/// The metadata key carrying an MCP tool call's remote **server identifier**
/// (`mcp_tool_use.server_name`). Required by the wire — Anthropic rejects an
/// `mcp_tool_use` block without it — so it is preserved under the `anthropic`
/// namespace and restored on render.
/// <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
const ANTHROPIC_MCP_SERVER_NAME: &str = "serverName";

/// Lift an Anthropic block/tool's `cache_control` object into a fresh
/// [`ProviderMetadata`] under `anthropic.cacheControl`. Anthropic's prompt
/// caching marks a cache breakpoint with `"cache_control":{"type":"ephemeral"}`
/// on a content block, a tool, or the system prompt; the object is preserved
/// verbatim so a same-protocol round-trip reproduces it exactly and a
/// cross-protocol route carries it (namespaced, ignored by other providers).
/// Returns an empty map when the value carries no `cache_control`.
/// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
fn parse_cache_control(value: &serde_json::Value) -> ProviderMetadata {
    let mut meta = ProviderMetadata::new();
    if let Some(cc) = value.get("cache_control").filter(|v| !v.is_null()) {
        set_provider_metadata(
            &mut meta,
            PROVIDER_ID_ANTHROPIC,
            ANTHROPIC_CACHE_CONTROL,
            cc.clone(),
        );
    }
    meta
}

/// The Anthropic `cache_control` object carried in `provider_metadata`, if any —
/// the inverse of [`parse_cache_control`]. Used by the render paths to splat
/// `cache_control` back onto the block/tool/system it belongs to.
fn cache_control_value(meta: &ProviderMetadata) -> Option<serde_json::Value> {
    provider_namespace(meta, PROVIDER_ID_ANTHROPIC)
        .and_then(|o| o.get(ANTHROPIC_CACHE_CONTROL))
        .cloned()
}

/// Splat the Anthropic `cache_control` object from `meta` onto an existing block
/// / tool / system JSON object (a no-op when there is none). Mutates in place so
/// the render sites can build the block first and stamp caching last.
fn apply_cache_control(target: &mut serde_json::Value, meta: &ProviderMetadata) {
    if let Some(cc) = cache_control_value(meta)
        && let Some(obj) = target.as_object_mut()
    {
        obj.insert("cache_control".to_string(), cc);
    }
}

/// Lift an Anthropic `thinking` / `redacted_thinking` block's continuity fields
/// into a [`ProviderMetadata`] under the `anthropic` namespace, alongside any
/// block-level `cache_control`. A `thinking` block carries a `signature`; a
/// `redacted_thinking` block carries an encrypted `data` payload and is marked
/// with a `redactedThinking: true` flag so the render path re-emits the correct
/// block type. Without this the signature is lost and Anthropic rejects a
/// follow-up turn that replays the (now-unsigned) thinking block.
/// <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
fn parse_reasoning_metadata(block: &serde_json::Value, block_type: &str) -> ProviderMetadata {
    let mut meta = parse_cache_control(block);
    if block_type == "redacted_thinking" {
        set_provider_metadata(
            &mut meta,
            PROVIDER_ID_ANTHROPIC,
            ANTHROPIC_REDACTED,
            serde_json::Value::Bool(true),
        );
        if let Some(data) = block.get("data") {
            set_provider_metadata(
                &mut meta,
                PROVIDER_ID_ANTHROPIC,
                ANTHROPIC_REDACTED_DATA,
                data.clone(),
            );
        }
    } else if let Some(sig) = block.get("signature").filter(|v| !v.is_null()) {
        set_provider_metadata(
            &mut meta,
            PROVIDER_ID_ANTHROPIC,
            ANTHROPIC_SIGNATURE,
            sig.clone(),
        );
    }
    meta
}

/// Render a canonical reasoning part back into its Anthropic block, inverting
/// [`parse_reasoning_metadata`]. A `redactedThinking` flag re-emits a
/// `redacted_thinking` block whose `data` is the preserved encrypted payload
/// (falling back to `text` when absent); otherwise a `thinking` block, carrying
/// its `signature` when one round-tripped.
///
/// A `cache_control` breakpoint is intentionally **not** re-applied here:
/// Anthropic rejects `cache_control` on `thinking` / `redacted_thinking` blocks
/// (they are cached implicitly when they appear in a prior assistant turn), so
/// the Vercel reference validates them with `canCache: false` and never emits
/// the field. Any `cacheControl` that rode in `provider_metadata` is therefore
/// dropped on render rather than producing a request the API would reject.
/// <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
fn render_reasoning_block(text: &str, meta: &ProviderMetadata) -> serde_json::Value {
    let ns = provider_namespace(meta, PROVIDER_ID_ANTHROPIC);
    let is_redacted = ns
        .and_then(|o| o.get(ANTHROPIC_REDACTED))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_redacted {
        // Prefer the preserved encrypted payload; the canonical `text` mirrors
        // it on parse, so it is the correct fallback if metadata was stripped.
        let data = ns
            .and_then(|o| o.get(ANTHROPIC_REDACTED_DATA))
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(text.to_string()));
        serde_json::json!({ "type": "redacted_thinking", "data": data })
    } else {
        let mut obj = serde_json::json!({ "type": "thinking", "thinking": text });
        if let Some(sig) = ns.and_then(|o| o.get(ANTHROPIC_SIGNATURE)) {
            obj["signature"] = sig.clone();
        }
        obj
    }
}

/// Build the `provider_metadata` for an Anthropic `mcp_tool_use` block: the MCP
/// marker (`type: "mcp-tool-use"`) plus the remote `server_name`, alongside any
/// block-level `cache_control`. The pair lets the render path reproduce the exact
/// `mcp_tool_use` block — and any non-Anthropic wire ignores the namespace.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/anthropic-messages-language-model.ts>
fn parse_mcp_tool_use_metadata(block: &serde_json::Value) -> ProviderMetadata {
    let mut meta = parse_cache_control(block);
    set_provider_metadata(
        &mut meta,
        PROVIDER_ID_ANTHROPIC,
        ANTHROPIC_MCP_TYPE,
        serde_json::Value::String(ANTHROPIC_MCP_TOOL_USE.to_string()),
    );
    if let Some(server) = block.get("server_name") {
        set_provider_metadata(
            &mut meta,
            PROVIDER_ID_ANTHROPIC,
            ANTHROPIC_MCP_SERVER_NAME,
            server.clone(),
        );
    }
    meta
}

/// The MCP `server_name` carried in `provider_metadata` (set by
/// [`parse_mcp_tool_use_metadata`]), if this part was tagged as an Anthropic MCP
/// tool-use. `None` means the part is not a renderable `mcp_tool_use` — either it
/// is an ordinary call, or it is a `dynamic` MCP call that arrived on a *different*
/// wire (e.g. an OpenAI `mcp_call`) and so has no Anthropic server name; such a
/// call degrades to a plain `tool_use` block, dropping the foreign server id.
fn mcp_server_name(meta: &ProviderMetadata) -> Option<String> {
    let ns = provider_namespace(meta, PROVIDER_ID_ANTHROPIC)?;
    let is_mcp = ns
        .get(ANTHROPIC_MCP_TYPE)
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == ANTHROPIC_MCP_TOOL_USE);
    if !is_mcp {
        return None;
    }
    ns.get(ANTHROPIC_MCP_SERVER_NAME)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Parse an Anthropic `mcp_tool_result` block's body into a [`ToolResultOutput`].
/// The wire `content` is the raw MCP result (a string or a `[{type:"text",
/// text}]` array); it is kept as a structured JSON value — `Json`, or `ErrorJson`
/// when `is_error` is set — so a same-protocol render re-emits the exact
/// `content`. (The AI SDK reference carries `result: part.content` and its render
/// only re-accepts a `json` / `error-json` body, which this mirrors.) A missing
/// `content` defaults to JSON `null`, the faithful empty value.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
fn parse_mcp_tool_result_output(block: &serde_json::Value) -> ToolResultOutput {
    let value = block
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let is_error = block
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    if is_error {
        ToolResultOutput::ErrorJson { value }
    } else {
        ToolResultOutput::Json { value }
    }
}

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
    /// Top-k sampling. Promoted to the canonical `top_k` slot so it translates
    /// across protocols (e.g. to a Gemini upstream's `generationConfig.topK`).
    /// <https://docs.anthropic.com/en/api/messages#body-top-k>
    #[serde(default)]
    top_k: Option<u32>,
    /// Custom stop sequences. Promoted to the canonical `stop` slot, so it can
    /// render as a Chat Completions `stop` or a Gemini `stopSequences`.
    /// <https://docs.anthropic.com/en/api/messages#body-stop-sequences>
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    stream: bool,
    /// Every other field — `tool_choice`, `metadata`, `thinking`, the deprecated
    /// flat `output_format` alias, … — rides along via `extra` and is splatted
    /// back on render. Skipped from the published schema so the documented
    /// contract is the set of typed fields; pass-through behavior is preserved
    /// at runtime.
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
fn parse_messages_tool(mut t: MessagesTool) -> Option<Tool> {
    // `cache_control` on a tool is a prompt-caching breakpoint, not a config
    // key — lift it out of `extra` (for both tool kinds) into the canonical
    // metadata slot so it round-trips and is not re-rendered as opaque `args`.
    // <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
    let mut provider_metadata = ProviderMetadata::new();
    if let Some(cc) = t.extra.remove("cache_control") {
        set_provider_metadata(
            &mut provider_metadata,
            PROVIDER_ID_ANTHROPIC,
            ANTHROPIC_CACHE_CONTROL,
            cc,
        );
    }
    if let Some(kind) = t.kind {
        // Server tool: `{type:"web_search_20250305", name:"web_search", …config}`.
        return Some(Tool::ProviderDefined {
            id: format!("{PROVIDER_ID_ANTHROPIC}.{kind}"),
            name: t.name.unwrap_or_else(|| kind.clone()),
            args: serde_json::Value::Object(t.extra.into_iter().collect()),
            provider_metadata,
        });
    }
    Some(Tool::Function {
        name: t.name?,
        description: t.description,
        parameters: t.input_schema,
        // Anthropic client tools carry no `strict` slot.
        strict: None,
        provider_metadata,
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
            provider_metadata,
        } => {
            let mut obj = serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            });
            // Restore a tool-level `cache_control` breakpoint when it round-tripped.
            apply_cache_control(&mut obj, provider_metadata);
            obj
        }
        Tool::ProviderDefined {
            id,
            name,
            args,
            provider_metadata,
        } => {
            let mut obj = provider_defined_native(id, name, args);
            apply_cache_control(&mut obj, provider_metadata);
            obj
        }
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

/// Lift a `cache_control` breakpoint off Anthropic's `system` field into a
/// [`ProviderMetadata`] under `anthropic.cacheControl`. The system prefix is the
/// highest-value, most common prompt-cache breakpoint; a string system carries
/// none, while an array system carries it on a `{type:"text", text,
/// cache_control}` block. The last block that carries `cache_control` wins — the
/// cache boundary is the cumulative prefix up to the final breakpoint.
/// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
fn parse_system_metadata(value: &serde_json::Value) -> ProviderMetadata {
    match value {
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .rev()
            .map(parse_cache_control)
            .find(|m| !m.is_empty())
            .unwrap_or_default(),
        // A plain-string system has no place to carry `cache_control`.
        _ => ProviderMetadata::new(),
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
        serde_json::Value::String(s) => Ok(vec![Content::Text {
            text: s.clone(),
            provider_metadata: ProviderMetadata::new(),
        }]),
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
                        // Preserve a block-level `cache_control` breakpoint.
                        provider_metadata: parse_cache_control(block),
                    }),
                    // both `thinking` and `redacted_thinking` map to Reasoning.
                    // The `signature` (thinking) and encrypted `data`
                    // (redacted_thinking) are continuity tokens with no canonical
                    // field — carry them in `provider_metadata` so a thinking
                    // block can be replayed on a follow-up turn.
                    // <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
                    "thinking" | "redacted_thinking" => out.push(Content::Reasoning {
                        text: block
                            .get("thinking")
                            .or_else(|| block.get("data"))
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        provider_metadata: parse_reasoning_metadata(block, block_type),
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
                        // Neither block is a runtime MCP (`dynamic`) tool call.
                        dynamic: false,
                        // Preserve a block-level `cache_control` breakpoint.
                        provider_metadata: parse_cache_control(block),
                    }),
                    // Anthropic `mcp_tool_use` block `{id, name, input, server_name}`
                    // — a provider-executed remote MCP tool call (the beta MCP
                    // connector). It maps to a `dynamic`, provider-executed
                    // `ToolCall`; the load-bearing `server_name` has no core field
                    // and rides in `provider_metadata["anthropic"]` (with the
                    // `type: "mcp-tool-use"` marker) so the render reproduces the
                    // exact block. Mirrors the AI SDK reference mapping.
                    // <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
                    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/anthropic-messages-language-model.ts>
                    "mcp_tool_use" => out.push(Content::ToolCall {
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
                        provider_executed: true,
                        dynamic: true,
                        provider_metadata: parse_mcp_tool_use_metadata(block),
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
                        // An ordinary `tool_result` is not an MCP inline result.
                        dynamic: false,
                        // Preserve a block-level `cache_control` breakpoint.
                        provider_metadata: parse_cache_control(block),
                    }),
                    // Anthropic `mcp_tool_result` block `{tool_use_id, content,
                    // is_error}` — the inline result of an `mcp_tool_use` call. It
                    // maps to a `dynamic` `ToolResult` whose body is the raw MCP
                    // `content` (a JSON value), kept as `Json` / `ErrorJson` so the
                    // render re-emits it verbatim; the AI SDK reference likewise
                    // carries `result: part.content` and only accepts a JSON body
                    // back. The `cache_control` breakpoint is preserved.
                    // <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
                    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/anthropic-messages-language-model.ts>
                    "mcp_tool_result" => out.push(Content::ToolResult {
                        call_id: block
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_name: None,
                        output: parse_mcp_tool_result_output(block),
                        dynamic: true,
                        provider_metadata: parse_cache_control(block),
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
                            // Preserve a block-level `cache_control` breakpoint.
                            provider_metadata: parse_cache_control(block),
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
        // Lift any system-block `cache_control` breakpoint into the parallel
        // metadata slot so the highest-value prompt-cache point (the system
        // prefix) survives — a bare `system: Option<String>` would flatten it.
        let system_provider_metadata = req
            .system
            .as_ref()
            .map(parse_system_metadata)
            .unwrap_or_default();

        let mut messages = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            let role = parse_role(&m.role)?;
            // A user-role message may carry client `tool_result` blocks — split
            // those into a canonical Tool-role message so the IR stays clean. A
            // `dynamic` MCP result (an `mcp_tool_result`), however, is part of the
            // assistant turn — it pairs with its `mcp_tool_use` call — so it is
            // kept in place (NOT partitioned out), and re-renders as an
            // `mcp_tool_result` block rather than a request-side `tool_result`.
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
            let parsed = parse_content(&m.content)?;
            let (tool_results, rest): (Vec<_>, Vec<_>) = parsed
                .into_iter()
                .partition(|c| matches!(c, Content::ToolResult { dynamic: false, .. }));
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
                // Anthropic's GA format carries no name / description / strict.
                response_format = Some(ResponseFormat::JsonSchema {
                    name: None,
                    description: None,
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

        // Promote a known-shape `tool_choice` into the canonical slot so it
        // translates across protocols (the v0 #547 bug: Anthropic's object form
        // reaching an OpenAI upstream verbatim). Unmapped shapes stay in `extra`.
        // Anthropic nests parallel-tool control inside the object as
        // `disable_parallel_tool_use`; the parser lifts it to the top-level
        // `parallel_tool_calls` in `extra` so it survives the round-trip.
        let tool_choice = parse_messages_tool_choice(&mut extra);

        Ok(Prompt {
            model: req.model,
            system,
            system_provider_metadata,
            messages,
            tools,
            params: GenerationParams {
                temperature: req.temperature,
                top_p: req.top_p,
                max_tokens: req.max_tokens,
                reasoning_effort,
                response_modalities: Vec::new(),
                top_k: req.top_k,
                // Anthropic carries no seed or penalties on its wire.
                seed: None,
                stop: req.stop_sequences.unwrap_or_default(),
                presence_penalty: None,
                frequency_penalty: None,
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
        // Prefer the stashed raw `stop_reason` (e.g. `stop_sequence`) over the
        // unified-enum mapping so a same-protocol round-trip is byte-faithful.
        let stop_reason =
            rendered_finish_reason(result, PROVIDER_ID_ANTHROPIC, finish_to_stop_reason)
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
            // When a system-prompt `cache_control` breakpoint round-tripped,
            // re-render `system` as a cached `[{type:"text", text,
            // cache_control}]` block (the Anthropic array form) rather than a
            // bare string, so the highest-value cache point is preserved. With no
            // breakpoint, the plain-string form is kept.
            // <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
            match cache_control_value(&prompt.system_provider_metadata) {
                Some(cc) => {
                    req.insert(
                        "system".into(),
                        serde_json::json!([{
                            "type": "text",
                            "text": system,
                            "cache_control": cc,
                        }]),
                    );
                }
                None => {
                    req.insert("system".into(), system.clone().into());
                }
            }
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
        // the extras splat so it wins over any leftover `tool_choice`. Anthropic
        // nests parallel-tool control inside the object, so fold the
        // protocol-neutral top-level `parallel_tool_calls` (the shape every other
        // protocol uses) back into `disable_parallel_tool_use` here.
        let parallel_tool_calls = prompt
            .params
            .extra
            .get("parallel_tool_calls")
            .and_then(|v| v.as_bool());
        if prompt.tool_choice.is_some() || parallel_tool_calls.is_some() {
            let mut tc = match &prompt.tool_choice {
                Some(tc) => render_messages_tool_choice(tc),
                // `disable_parallel_tool_use` only exists on a tool_choice object;
                // default to `auto` (Anthropic's own default) to carry it.
                None => serde_json::json!({ "type": "auto" }),
            };
            if let Some(parallel) = parallel_tool_calls
                && let Some(obj) = tc.as_object_mut()
            {
                obj.insert("disable_parallel_tool_use".into(), (!parallel).into());
            }
            req.insert("tool_choice".into(), tc);
        }
        // Render the typed sampling slots Anthropic carries: `top_k` and
        // `stop_sequences`. Seed and presence/frequency penalties have no
        // Messages wire field, so they are intentionally not rendered here.
        // <https://docs.anthropic.com/en/api/messages>
        if let Some(top_k) = prompt.params.top_k {
            req.insert("top_k".into(), top_k.into());
        }
        if !prompt.params.stop.is_empty() {
            req.insert("stop_sequences".into(), prompt.params.stop.clone().into());
        }
        // Splat anthropic-specific extras (metadata, thinking, …) back into
        // the outbound request. `parallel_tool_calls` is skipped — it was folded
        // into `tool_choice` above, and Anthropic has no top-level field for it.
        // Typed fields win over same-named extras.
        for (k, v) in &prompt.params.extra {
            if k == "parallel_tool_calls" {
                continue;
            }
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
                        provider_metadata: parse_cache_control(block),
                    });
                    // A text block may carry inline web-search citations; lift
                    // them into `Content::Source` parts right after the text.
                    content.extend(parse_messages_block_sources(block, &mut source_index));
                }
                Some(thinking @ ("thinking" | "redacted_thinking")) => {
                    content.push(Content::Reasoning {
                        text: block
                            .get("thinking")
                            .or_else(|| block.get("data"))
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        // Carry the thinking `signature` / redacted `data` so the
                        // reasoning block can be replayed on a follow-up turn.
                        provider_metadata: parse_reasoning_metadata(block, thinking),
                    });
                }
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
                    // Neither block is a runtime MCP (`dynamic`) tool call.
                    dynamic: false,
                    provider_metadata: parse_cache_control(block),
                }),
                // An `mcp_tool_use` block — a provider-executed remote MCP tool
                // call on the response side; same mapping as the request parse
                // above (`dynamic`, provider-executed, server identity in
                // `provider_metadata["anthropic"]`).
                // <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
                Some("mcp_tool_use") => content.push(Content::ToolCall {
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
                    provider_executed: true,
                    dynamic: true,
                    provider_metadata: parse_mcp_tool_use_metadata(block),
                }),
                // An `mcp_tool_result` block — the inline result of an
                // `mcp_tool_use` call, emitted in the assistant turn right after
                // the call. Mapped to a `dynamic` `ToolResult` carrying the raw MCP
                // `content`, paired by `tool_use_id`.
                // <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
                Some("mcp_tool_result") => content.push(Content::ToolResult {
                    call_id: block
                        .get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    tool_name: None,
                    output: parse_mcp_tool_result_output(block),
                    dynamic: true,
                    provider_metadata: parse_cache_control(block),
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
        let raw_stop = body
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let finish_reason = raw_stop.as_deref().and_then(stop_reason_to_finish);
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
        // Anthropic's Messages response carries no result-level metadata field
        // without a dedicated canonical slot (unlike OpenAI's
        // `system_fingerprint` or Gemini's `modelVersion`); the per-block
        // signature / cache_control above are where its provider metadata lives.
        // The one result-level value we preserve is the raw `stop_reason` when
        // the unified enum can't reproduce it — `stop_sequence` (→ `Stop`, which
        // renders `end_turn`) and `refusal` are the lossy cases — so a
        // same-protocol render restores the exact native reason.
        let mut provider_metadata = ProviderMetadata::new();
        stash_raw_finish_reason(
            &mut provider_metadata,
            PROVIDER_ID_ANTHROPIC,
            raw_stop.as_deref(),
            finish_reason.as_ref(),
            finish_to_stop_reason,
        );
        Ok(GenerateResult {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details,
            provider_metadata,
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
        description: None,
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

/// Promote an Anthropic `tool_choice` into the canonical [`ToolChoice`], removing
/// it from `extra` when it maps to a known shape. Unmapped shapes are left
/// untouched so they pass through opaquely. Anthropic nests parallel-tool control
/// inside the object as `disable_parallel_tool_use`; lift it to the
/// protocol-neutral top-level `parallel_tool_calls` (the shape every other
/// protocol uses) so it survives translation rather than being dropped with the
/// consumed object.
/// <https://docs.anthropic.com/en/api/messages> → `tool_choice`.
fn parse_messages_tool_choice(
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) -> Option<ToolChoice> {
    let (parsed, disable_parallel) = {
        let obj = extra.get("tool_choice").and_then(|v| v.as_object())?;
        let parsed = match obj.get("type").and_then(|t| t.as_str()) {
            Some("auto") => Some(ToolChoice::Auto),
            Some("any") => Some(ToolChoice::Required),
            Some("none") => Some(ToolChoice::None),
            Some("tool") => obj
                .get("name")
                .and_then(|n| n.as_str())
                .map(|name| ToolChoice::Tool {
                    name: name.to_string(),
                }),
            _ => None,
        };
        let disable_parallel = obj
            .get("disable_parallel_tool_use")
            .and_then(|v| v.as_bool());
        (parsed, disable_parallel)
    };
    if parsed.is_some() {
        if let Some(disable) = disable_parallel {
            extra.insert("parallel_tool_calls".to_string(), (!disable).into());
        }
        extra.remove("tool_choice");
    }
    parsed
}

/// Render the canonical [`ToolChoice`] into Anthropic's native `tool_choice`
/// object: `{type:"auto"|"any"|"none"}` or `{type:"tool", name}`.
fn render_messages_tool_choice(tc: &ToolChoice) -> serde_json::Value {
    match tc {
        ToolChoice::Auto => serde_json::json!({ "type": "auto" }),
        ToolChoice::Required => serde_json::json!({ "type": "any" }),
        ToolChoice::None => serde_json::json!({ "type": "none" }),
        ToolChoice::Tool { name } => serde_json::json!({ "type": "tool", "name": name }),
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
    // A `web_search_tool_result` block carries the originating `server_tool_use`
    // id that paired it on the wire; preserve it so the render path restores the
    // EXACT pairing id rather than reusing one by position.
    // <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty());
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
        let mut provider_metadata = ProviderMetadata::new();
        if let Some(id) = tool_use_id {
            set_provider_metadata(
                &mut provider_metadata,
                PROVIDER_ID_ANTHROPIC,
                ANTHROPIC_TOOL_USE_ID,
                serde_json::Value::String(id.to_string()),
            );
        }
        out.push(Content::Source {
            source: Source::Url {
                id: Source::synthesize_id(url, *next_index),
                url: url.to_string(),
                title,
            },
            provider_metadata,
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
/// **Pairing strategy.** On the Anthropic wire a `web_search_tool_result` is
/// invalid on its own: it must pair with a `server_tool_use` block sharing one
/// `tool_use_id`, or a client echoing the assistant turn into a follow-up
/// triggers `invalid_request_error`. The `tool_use_id` is resolved in priority
/// order:
/// 1. **Real call present** — the upstream `server_tool_use` parsed to a
///    `ToolCall{provider_executed:true, name:"web_search"}` and is re-rendered as
///    a real `server_tool_use` by [`render_content_block`]. Reuse its id as
///    `tool_use_id` (real pairing; no second `server_tool_use` is emitted).
/// 2. **Exact id preserved** — no originating call survived (e.g. a cross-protocol
///    route, or a result that carried only the citations), but the `Source`'s
///    `provider_metadata["anthropic"]["toolUseId"]` preserved the *exact*
///    originating id from parse. Synthesize a `server_tool_use` carrying that
///    exact id, so the pair reproduces the original `tool_use_id` byte-for-byte.
/// 3. **Fallback** — no id at all (a genuinely foreign source, e.g. Gemini
///    grounding): synthesize a `server_tool_use` with a stable placeholder id so
///    the emitted pair is at least valid.
///
/// [`Source::Document`] citations have no `web_search_result` form and are
/// skipped here.
/// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
fn render_web_search_result_blocks(result: &GenerateResult) -> Vec<serde_json::Value> {
    let results: Vec<serde_json::Value> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Source {
                source: Source::Url { url, title, .. },
                ..
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
    // (1) Borrow the id of the originating provider-executed `web_search` call.
    // All URL sources collapse into one result block, and Anthropic emits one
    // web-search call per result block, so the last such call in the content is
    // the originator. That call is already re-rendered as a real
    // `server_tool_use` by `render_content_block`, so reusing its id keeps the
    // pair correlated without emitting a duplicate (or orphaning the real call).
    let call_id = result.content.iter().rev().find_map(|c| match c {
        Content::ToolCall { id, .. } if is_web_search_call(c) => Some(id.clone()),
        _ => None,
    });
    if let Some(id) = call_id {
        return vec![serde_json::json!({
            "type": "web_search_tool_result",
            "tool_use_id": id,
            "content": results,
        })];
    }
    // (2) No surviving call — fall back to the EXACT originating `tool_use_id`
    // preserved on a `Source`'s provider metadata, if any, so the pair restores
    // the original id rather than the placeholder. (3) Otherwise the placeholder.
    let preserved_id = result.content.iter().find_map(|c| match c {
        Content::Source {
            provider_metadata, ..
        } => provider_namespace(provider_metadata, PROVIDER_ID_ANTHROPIC)
            .and_then(|o| o.get(ANTHROPIC_TOOL_USE_ID))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    });
    let tool_use_id = preserved_id.unwrap_or_else(|| SYNTHETIC_WEB_SEARCH_ID.to_string());
    vec![
        serde_json::json!({
            "type": "server_tool_use",
            "id": tool_use_id,
            "name": ANTHROPIC_WEB_SEARCH_TOOL,
            "input": {},
        }),
        serde_json::json!({
            "type": "web_search_tool_result",
            "tool_use_id": tool_use_id,
            "content": results,
        }),
    ]
}

fn render_content_block(c: &Content) -> Option<serde_json::Value> {
    match c {
        Content::Text {
            text,
            provider_metadata,
        } => {
            let mut block = serde_json::json!({ "type": "text", "text": text });
            apply_cache_control(&mut block, provider_metadata);
            Some(block)
        }
        Content::Reasoning {
            text,
            provider_metadata,
        } => Some(render_reasoning_block(text, provider_metadata)),
        Content::ToolCall {
            id,
            name,
            arguments,
            provider_executed,
            dynamic,
            provider_metadata,
        } => {
            let input: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
            // A `dynamic` provider-executed MCP call that carries an Anthropic
            // `server_name` (the `mcp-tool-use` metadata marker) reproduces its
            // native `mcp_tool_use` block, restoring the load-bearing server id.
            // The AI SDK gates the same way on `providerOptions.anthropic.type ===
            // 'mcp-tool-use'` and warns/drops when the server name is missing; here
            // a dynamic call WITHOUT an Anthropic server name (e.g. one routed in
            // from an OpenAI `mcp_call`) simply degrades to a plain `tool_use`,
            // dropping the foreign server identity.
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
            if *dynamic
                && *provider_executed
                && let Some(server_name) = mcp_server_name(provider_metadata)
            {
                let mut block = serde_json::json!({
                    "type": "mcp_tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                    "server_name": server_name,
                });
                apply_cache_control(&mut block, provider_metadata);
                return Some(block);
            }
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
            let mut block = serde_json::json!({
                "type": block_type, "id": id, "name": name, "input": input,
            });
            apply_cache_control(&mut block, provider_metadata);
            Some(block)
        }
        // A `dynamic` MCP tool result is part of the assistant turn — it follows
        // its `mcp_tool_use` call as an `mcp_tool_result` block (rather than a
        // request-side `tool_result`). Re-emit that block, carrying the raw MCP
        // `content` back from the structured output and the `is_error` flag. The
        // AI SDK reference only round-trips a JSON-bodied MCP result, so a
        // non-`Json`/`ErrorJson` output (only reachable from a hand-built value or
        // a cross-protocol route) has no faithful `mcp_tool_result` form and is
        // dropped here — exactly as the reference warns-and-skips it.
        // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
        Content::ToolResult {
            call_id,
            output,
            dynamic,
            provider_metadata,
            ..
        } if *dynamic => {
            let (value, is_error) = match output {
                ToolResultOutput::Json { value } => (value.clone(), false),
                ToolResultOutput::ErrorJson { value } => (value.clone(), true),
                _ => return None,
            };
            let mut block = serde_json::json!({
                "type": "mcp_tool_result",
                "tool_use_id": call_id,
                "is_error": is_error,
                "content": value,
            });
            apply_cache_control(&mut block, provider_metadata);
            Some(block)
        }
        // Ordinary tool results are request-side only; not part of an assistant
        // reply (they ride a Tool-role message rendered by `render_message`).
        Content::ToolResult { .. } => None,
        // image/* -> an `image` block, everything else -> a `document` block.
        // Source is `{type:base64,media_type,data}` or `{type:url,url}`.
        // <https://docs.anthropic.com/en/docs/build-with-claude/vision>
        Content::File {
            media_type,
            data,
            provider_metadata,
            ..
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
            let mut block = serde_json::json!({ "type": block_type, "source": source });
            apply_cache_control(&mut block, provider_metadata);
            Some(block)
        }
        // Citations are not a per-block render: they are collected across the
        // whole reply and re-attached as a `server_tool_use` ↔
        // `web_search_tool_result` pair by `render_response` (see
        // `render_web_search_result_blocks`). Skip here.
        Content::Source { .. } => None,
        // The Anthropic Messages wire has no tool-approval handshake: it carries
        // no `mcp_approval_request` / `mcp_approval_response` block. The AI SDK's
        // Anthropic converter likewise drops a `tool-approval-response` part
        // (`continue`), so both approval parts are skipped here. A denied
        // execution still degrades to a plain `tool_result` string on the
        // request side (`render_tool_result_content` via `to_provider_string`).
        // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
        Content::ToolApprovalRequest { .. } | Content::ToolApprovalResponse { .. } => None,
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
                    call_id,
                    output,
                    provider_metadata,
                    ..
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
                    // Restore a `tool_result`-level `cache_control` breakpoint.
                    apply_cache_control(&mut block, provider_metadata);
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
    // No message-level `cache_control` is emitted: Anthropic rejects
    // `cache_control` as a sibling of `role`/`content`. A turn-level cache
    // breakpoint belongs on a content block, and the Vercel reference folds a
    // message's `cacheControl` onto its last content block rather than the
    // message object — so the per-block render above already covers caching.
    // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
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
    // Anthropic reports provider-executed server-tool counts under
    // `usage.server_tool_use`. Web search is the only count today.
    // <https://docs.anthropic.com/en/api/messages> → `usage` object.
    let web_search_count = value
        .get("server_tool_use")
        .and_then(|s| s.get("web_search_requests"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: input.saturating_add(cache_read).saturating_add(cache_write),
        completion_tokens: output,
        reasoning_tokens: 0,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        web_search_count,
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
    /// per content-block index → its canonical kind, so `content_block_stop`
    /// can emit the matching [`StreamPart::TextEnd`] / [`StreamPart::ReasoningEnd`]
    /// for the block that just closed. `None` for an index never opened or for a
    /// block kind that carries no lifecycle marker (tool / web-search results).
    block_kinds: Vec<Option<BlockKind>>,
    /// per content-block index → the thinking block's `signature`, accumulated
    /// from its terminal `signature_delta` so `content_block_stop` can attach it
    /// to the emitted [`StreamPart::ReasoningEnd`] (reasoning continuity — without
    /// it a replayed thinking block is unsigned and Anthropic rejects it).
    block_signatures: Vec<Option<String>>,
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
                        if let Content::Source { source, .. } = content {
                            parts.push(StreamPart::Source { source });
                        }
                    }
                    return Ok(parts);
                }
                // Streaming MCP gap: a streamed `mcp_tool_use` block (a
                // provider-executed remote MCP call) and its companion
                // `mcp_tool_result` are not recognised here and fall to
                // `BlockKind::Text`, so they are mishandled on this delta path —
                // the streaming counterpart of the `web_search` `citations_delta`
                // gap documented above. Surfacing them faithfully would require
                // new cross-protocol `StreamPart` plumbing (the other encoders
                // have no streaming MCP frame to target). The non-streaming
                // handshake (`parse_response` / `render_response`) is the
                // complete, faithful round trip; the streamed blocks are a
                // deferred gap, not yet wired.
                // <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
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
                while self.block_kinds.len() <= index {
                    self.block_kinds.push(None);
                }
                self.block_kinds[index] = Some(kind);
                match kind {
                    // Text / thinking blocks carry an explicit lifecycle on this
                    // wire; surface the open as a canonical start marker so a
                    // framing client encoder reopens a fresh block (the
                    // merged-block fix). The block id is Anthropic's numeric
                    // index rendered as a string.
                    BlockKind::Text => parts.push(StreamPart::TextStart {
                        id: index.to_string(),
                    }),
                    BlockKind::Thinking => parts.push(StreamPart::ReasoningStart {
                        id: index.to_string(),
                    }),
                    BlockKind::ToolUse => {
                        // A tool block's open already frames itself via the
                        // `name`-bearing `ToolCallDelta` below — no separate
                        // start marker (see the `StreamPart` enum docs).
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
                    Some("signature_delta") => {
                        // The thinking block's continuity signature, emitted just
                        // before its `content_block_stop`. Stash it per-index so
                        // the `ReasoningEnd` for this block can carry it; it does
                        // not map to a delta part of its own.
                        if let Some(sig) = delta
                            .and_then(|d| d.get("signature"))
                            .and_then(|s| s.as_str())
                        {
                            while self.block_signatures.len() <= index {
                                self.block_signatures.push(None);
                            }
                            self.block_signatures[index] = Some(sig.to_string());
                        }
                    }
                    // any other delta type: ignore, do not error
                    _ => {}
                }
            }
            "content_block_stop" => {
                // Close the matching text / reasoning block so the boundary
                // survives re-encoding. Tool / web-search blocks carry no end
                // marker (their framing is the `name`-bearing delta on open and
                // the next block / terminal on close).
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                match self.block_kinds.get(index).copied().flatten() {
                    Some(BlockKind::Text) => parts.push(StreamPart::TextEnd {
                        id: index.to_string(),
                    }),
                    Some(BlockKind::Thinking) => parts.push(StreamPart::ReasoningEnd {
                        id: index.to_string(),
                        signature: self.block_signatures.get(index).cloned().flatten(),
                    }),
                    _ => {}
                }
            }
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
                    // Cumulative final count — assign, do not accumulate
                    // (same pattern as the sibling token fields above).
                    if let Some(wsc) = u
                        .get("server_tool_use")
                        .and_then(|s| s.get("web_search_requests"))
                        .and_then(|v| v.as_u64())
                    {
                        self.usage.web_search_count = wsc;
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

/// Map a router tool-result output to an Anthropic `mcp_tool_result`
/// `content` value (an array of content blocks, or a raw JSON body) plus the
/// `is_error` flag. Mirrors the non-streaming `mcp_tool_result` render.
fn server_tool_result_content(output: &ToolResultOutput) -> (serde_json::Value, bool) {
    match output {
        ToolResultOutput::Text { value } => (
            serde_json::json!([{ "type": "text", "text": value }]),
            false,
        ),
        ToolResultOutput::ErrorText { value } => {
            (serde_json::json!([{ "type": "text", "text": value }]), true)
        }
        // `mcp_tool_result.content` must be a string or an array of content
        // blocks — a non-text MCP result (a raw JSON body) is wrapped in a text
        // block rather than passed through as a bare object.
        ToolResultOutput::Json { value } => (
            serde_json::json!([{ "type": "text", "text": value.to_string() }]),
            false,
        ),
        ToolResultOutput::ErrorJson { value } => (
            serde_json::json!([{ "type": "text", "text": value.to_string() }]),
            true,
        ),
        ToolResultOutput::Content { value } => {
            let parts: Vec<serde_json::Value> = value
                .iter()
                .filter_map(|p| match p {
                    ToolResultContentPart::Text { text } => {
                        Some(serde_json::json!({ "type": "text", "text": text }))
                    }
                    _ => None,
                })
                .collect();
            (serde_json::Value::Array(parts), false)
        }
        ToolResultOutput::ExecutionDenied { reason } => (
            serde_json::json!([{
                "type": "text",
                "text": format!("execution denied: {}", reason.as_deref().unwrap_or("")),
            }]),
            true,
        ),
    }
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

    /// Force-open a *fresh* text / thinking block, emitting its
    /// `content_block_start`. Unlike [`Self::ensure_block_open`] this always
    /// closes any currently-open block first, so an explicit canonical
    /// [`StreamPart::TextStart`] / [`StreamPart::ReasoningStart`] starts a new
    /// content block at the next index even when one of the same kind was
    /// already open — this is the merged-block fix on this wire. A `text` block
    /// opens with an empty `text`, a `thinking` block with empty `thinking`,
    /// matching `content_block_start` on each kind.
    /// <https://docs.anthropic.com/en/api/messages-streaming>
    fn open_fresh_block(&mut self, frames: &mut Vec<SseFrame>, kind: EncoderBlockKind) {
        self.close_block(frames);
        self.block_kind = Some(kind);
        self.block_open = true;
        let content_block = match kind {
            EncoderBlockKind::Text => serde_json::json!({ "type": "text", "text": "" }),
            EncoderBlockKind::Thinking => serde_json::json!({ "type": "thinking", "thinking": "" }),
            // Tool blocks frame themselves via the `name`-bearing `ToolCallDelta`
            // arm, never through a start marker; keep the payload self-consistent
            // if a future caller routes one here.
            EncoderBlockKind::ToolUse => {
                serde_json::json!({ "type": "tool_use", "id": "", "name": "", "input": {} })
            }
        };
        frames.push(Self::ev(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": content_block,
            }),
        ));
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
            StreamPart::TextStart { .. } => {
                // Explicit block boundary from a block-framed upstream — open a
                // *fresh* text content block so two distinct upstream text blocks
                // re-encode as two `content_block_start`s, not one merged block.
                self.open_fresh_block(&mut frames, EncoderBlockKind::Text);
            }
            StreamPart::ReasoningStart { .. } => {
                self.open_fresh_block(&mut frames, EncoderBlockKind::Thinking);
            }
            StreamPart::TextEnd { .. } => {
                // Close the block the matching start opened; `close_block` is a
                // no-op if nothing is open, so a stray end is harmless.
                self.close_block(&mut frames);
            }
            StreamPart::ReasoningEnd { signature, .. } => {
                // Re-emit the thinking block's continuity signature as a
                // `signature_delta` (Anthropic's wire shape) just before its
                // `content_block_stop`, so a streamed thinking block re-encodes
                // signed and a follow-up turn replays it without an
                // "Invalid `signature`" rejection. Only when a thinking block is
                // actually open — a stray/empty reasoning-end stays a no-op.
                // <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
                if let Some(sig) = signature
                    && self.block_open
                    && self.block_kind == Some(EncoderBlockKind::Thinking)
                {
                    frames.push(Self::ev(
                        "content_block_delta",
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": { "type": "signature_delta", "signature": sig },
                        }),
                    ));
                }
                self.close_block(&mut frames);
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
                if let Some(name) = name.as_deref().filter(|n| !n.is_empty()) {
                    // A new tool call always opens its own block, even if a
                    // tool_use block was already open (consecutive tool calls
                    // are distinct blocks). Force-close, then open. Only a
                    // *non-empty* name opens a block: some upstreams re-send
                    // `name:""` on every argument-continuation delta, and
                    // treating `Some("")` as a new call would fragment one tool
                    // call into one empty-named `tool_use` block per delta.
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
            // A router-executed tool call rendered as a whole server-tool block:
            // a named MCP server -> `mcp_tool_use` (carrying `server_name`), else
            // the generic `server_tool_use`. Either marks the call
            // provider-executed, so the client never re-runs it. Opened and
            // closed as one block, mirroring `flush_pending_sources`.
            StreamPart::ServerToolCall {
                id,
                name,
                arguments,
                server_name,
                dynamic,
            } => {
                self.close_block(&mut frames);
                // Open with an empty input; the arguments stream as an
                // `input_json_delta`, matching how Anthropic frames a tool_use
                // block (the client builds `input` from the deltas).
                let content_block = match server_name {
                    Some(server) if *dynamic => serde_json::json!({
                        "type": "mcp_tool_use",
                        "id": id, "name": name, "server_name": server, "input": {},
                    }),
                    _ => serde_json::json!({
                        "type": "server_tool_use", "id": id, "name": name, "input": {},
                    }),
                };
                frames.push(Self::ev(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": content_block,
                    }),
                ));
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
                frames.push(Self::ev(
                    "content_block_stop",
                    serde_json::json!({ "type": "content_block_stop", "index": self.block_index }),
                ));
                self.block_index += 1;
            }
            // The matching result, rendered as a whole `mcp_tool_result` block.
            StreamPart::ServerToolResult {
                call_id, output, ..
            } => {
                self.close_block(&mut frames);
                let (content, is_error) = server_tool_result_content(output);
                frames.push(Self::ev(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {
                            "type": "mcp_tool_result",
                            "tool_use_id": call_id,
                            "is_error": is_error,
                            "content": content,
                        },
                    }),
                ));
                frames.push(Self::ev(
                    "content_block_stop",
                    serde_json::json!({ "type": "content_block_stop", "index": self.block_index }),
                ));
                self.block_index += 1;
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

#[cfg(test)]
mod tests {
    #[test]
    fn parse_usage_extracts_web_search_requests() {
        let v = serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "server_tool_use": { "web_search_requests": 3 }
        });
        let u = super::parse_usage(&v).expect("usage present");
        assert_eq!(u.web_search_count, 3);
        assert_eq!(u.prompt_tokens, 100);
    }

    #[test]
    fn parse_usage_web_search_defaults_zero() {
        let v = serde_json::json!({ "input_tokens": 10, "output_tokens": 2 });
        assert_eq!(super::parse_usage(&v).unwrap().web_search_count, 0);
    }
}
