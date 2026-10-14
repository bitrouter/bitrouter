//! Public messages request contracts.

use crate::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Messages request body (<https://docs.anthropic.com/en/api/messages>).
///
/// Public so clients and extensions can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessagesRequest {
    pub model: String,
    /// String or an array of `{type:"text", text}` blocks (#227 → #228).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<MessagesMessage>,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub tools: Vec<MessagesTool>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub temperature: Option<f64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub top_p: Option<f64>,
    /// Structured-output + reasoning config — `format` (`json_schema`) is
    /// promoted to the canonical `response_format` and `effort` to the canonical
    /// reasoning effort. Sibling keys pass through via `extra`.
    /// <https://platform.claude.com/docs/en/build-with-claude/structured-outputs>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub output_config: Option<MessagesOutputConfig>,
    /// Top-k sampling. Promoted to the canonical `top_k` slot so it translates
    /// across protocols (e.g. to a Gemini upstream's `generationConfig.topK`).
    /// <https://docs.anthropic.com/en/api/messages#body-top-k>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub top_k: Option<u32>,
    /// Custom stop sequences. Promoted to the canonical `stop` slot, so it can
    /// render as a Chat Completions `stop` or a Gemini `stopSequences`.
    /// <https://docs.anthropic.com/en/api/messages#body-stop-sequences>
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: bool,
    /// Every other field — `tool_choice`, `metadata`, `thinking`, the deprecated
    /// flat `output_format` alias, … — rides along via `extra` and is splatted
    /// back on render. Skipped from the published schema so the documented
    /// contract is the set of typed fields; pass-through behavior is preserved
    /// at runtime.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Messages `output_config` — structured output + reasoning effort.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessagesOutputConfig {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub format: Option<MessagesOutputFormat>,
    /// Reasoning effort (`low` | `medium` | `high` | `xhigh` | `max`).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub effort: Option<ReasoningEffort>,
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Messages `output_config.format`
/// (<https://platform.claude.com/docs/en/build-with-claude/structured-outputs>).
/// Anthropic's GA structured-output format carries no name / strict.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagesOutputFormat {
    /// JSON constrained to a schema.
    JsonSchema {
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// One element of [`MessagesRequest`]'s `messages` array — a `{ role, content }` turn.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessagesMessage {
    pub role: String,
    /// String or an array of content blocks.
    pub content: serde_json::Value,
    /// Unmodelled wire fields retained for service validation and round trips.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
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
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MessagesTool {
    #[serde(default, rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub kind: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub name: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: serde_json::Value,
    /// Server-tool configuration keys (e.g. `max_uses`, `allowed_domains`,
    /// `display_width_px`) for a provider-defined tool. Preserved verbatim.
    /// Skipped from the published schema — the documented contract is the typed
    /// client-tool shape.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}
