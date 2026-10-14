//! Public responses request contracts.

use crate::types::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Responses request body
/// (<https://platform.openai.com/docs/api-reference/responses/create>).
///
/// Public so clients and extensions can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
///
/// `input` stays `serde_json::Value` on purpose — the Responses API accepts
/// either a plain string or a heterogeneous item array (Codex multi-turn,
/// #454-3), and the deeper walk happens in `parse_input` (private). Mirrors
/// how Anthropic's `system` / `content` are typed.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponsesRequest {
    #[serde(default)]
    pub model: String,
    pub input: serde_json::Value,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub instructions: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub tools: Vec<ResponsesTool>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub temperature: Option<f64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub top_p: Option<f64>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub reasoning: Option<ResponsesReasoningConfig>,
    /// Output `text` config — `format` is the structured-output constraint
    /// (`json_schema` is promoted to the canonical slot; `text` / `json_object`
    /// pass through). Sibling keys (e.g. `verbosity`) survive via `extra`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    #[cfg_attr(feature = "schema", schemars(transform = super::omit_serialized_default))]
    pub text: Option<ResponsesText>,
    #[serde(default)]
    pub stream: bool,
    /// Whether OpenAI may retain the request and response.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub store: Option<bool>,
    /// Whether the model may issue tool calls in parallel.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub parallel_tool_calls: Option<bool>,
    /// Every other field — `tool_choice`,
    /// `max_tool_calls`, `metadata`, `include[]`, `previous_response_id`,
    /// `stream_options`, … — rides through here and is splatted
    /// back on render. Skipped from the published schema so the documented
    /// contract is the set of typed fields; pass-through behavior is
    /// preserved at runtime.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Responses `text` config. `format` constrains output shape; sibling keys
/// (e.g. `verbosity`) pass through via `extra`.
#[derive(Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponsesText {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub format: Option<ResponsesTextFormat>,
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Responses `text.format`
/// (<https://platform.openai.com/docs/api-reference/responses/create>) — a
/// closed union over OpenAI's three output modes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesTextFormat {
    /// Free-form text — the default.
    Text,
    /// Legacy JSON mode.
    JsonObject,
    /// JSON constrained to a schema.
    JsonSchema {
        /// Schema name (OpenAI requires it).
        name: String,
        /// Optional schema description — extra LLM guidance OpenAI passes to the
        /// model. Promoted to the canonical `response_format` description.
        /// <https://platform.openai.com/docs/api-reference/responses/create>
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Strict-mode flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// One element of [`ResponsesRequest`]'s `tools` array.
///
/// Responses uses one flat array for both kinds of tool:
/// - a **function** tool — `{type:"function", name, description?, parameters, strict?}`;
/// - a **provider-defined** server tool — `{type:"web_search_preview"|…, …config}`
///   (`code_interpreter`, `file_search`, `image_generation`, `computer_use_preview`).
///
/// `kind` (`type`) discriminates the two; for a server tool the configuration
/// keys ride in `extra` so they are preserved verbatim into the canonical
/// `Tool::ProviderDefined.args`.
/// <https://platform.openai.com/docs/api-reference/responses/create#responses-create-tools>
#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponsesTool {
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
    pub parameters: serde_json::Value,
    /// OpenAI strict-mode flag (V3 `strict`) on a function tool. Captured so it
    /// is not lost across the canonical boundary.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub strict: Option<bool>,
    /// Server-tool configuration keys (e.g. `search_context_size`,
    /// `container`, `vector_store_ids`) for a provider-defined tool. Preserved
    /// verbatim. Skipped from the published schema — the documented contract is
    /// the typed function-tool shape.
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}

/// `reasoning` knob on [`ResponsesRequest`]. `effort` is promoted to the
/// canonical slot; sibling fields such as `summary` round-trip via `extra`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResponsesReasoningConfig {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(!skip_serializing_if))]
    pub effort: Option<ReasoningEffort>,
    #[serde(flatten)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub extra: HashMap<String, serde_json::Value>,
}
