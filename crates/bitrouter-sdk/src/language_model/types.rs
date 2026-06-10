//! Core data types for the `language_model` protocol: the canonical internal
//! representation (`Prompt` / `StreamPart` / `GenerateResult`) plus routing and
//! pipeline I/O types.
//!
//! These are deliberately minimal in Phase 1 — Phase 2 fills in the full
//! protocol-conversion surface (tool calls, reasoning variants, content blocks).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::caller::CallerContext;

/// A uniform, namespaced metadata slot carried on every content part, message,
/// tool, and result — a faithful port of the Vercel AI SDK V3
/// `SharedV3ProviderMetadata` / `SharedV3ProviderOptions`
/// (`Record<string, Record<string, JSONValue>>`): the outer map is keyed by a
/// provider id (`"anthropic"`, `"openai"`, `"google"`, …) and the inner object
/// holds that provider's metadata for the part. On the AI SDK the input form is
/// `providerOptions` and the output form is `providerMetadata`; both share this
/// one wire shape, so the canonical IR carries a single `provider_metadata` slot
/// in both directions.
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/shared/v3/shared-v3-provider-metadata.ts>
///
/// A [`BTreeMap`] (not a `HashMap`) so serialization is deterministic — the same
/// metadata always renders in the same key order, which keeps round-trip tests
/// and request hashing stable.
///
/// **Namespacing is load-bearing for cross-protocol routing.** Each adapter
/// reads and writes **only its own** provider id's entry (see
/// [`PROVIDER_ID_ANTHROPIC`](crate::language_model::protocol) etc.) and leaves
/// every other namespace untouched, so e.g. an Anthropic `cacheControl` hint
/// survives verbatim under `provider_metadata["anthropic"]` even when the part
/// is routed to an OpenAI upstream (which simply ignores a foreign namespace).
pub type ProviderMetadata = BTreeMap<String, serde_json::Value>;

/// Read a single provider namespace's metadata object out of a
/// [`ProviderMetadata`] map — the `{key: JSONValue}` inner record an adapter
/// owns. Returns `None` when the provider has no entry, so a render path can
/// cheaply skip parts that carry nothing for it.
pub(crate) fn provider_namespace<'a>(
    meta: &'a ProviderMetadata,
    provider_id: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    meta.get(provider_id).and_then(|v| v.as_object())
}

/// Insert `value` under `key` within `provider_id`'s namespace in a
/// [`ProviderMetadata`] map, creating the namespace object on first write. The
/// single mutation primitive every adapter uses to lift a provider-native hint
/// (Anthropic `cacheControl`, a reasoning `signature`, an OpenAI image `detail`,
/// …) into the canonical slot without disturbing other providers' namespaces.
pub(crate) fn set_provider_metadata(
    meta: &mut ProviderMetadata,
    provider_id: &str,
    key: &str,
    value: serde_json::Value,
) {
    match meta
        .entry(provider_id.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
    {
        Some(obj) => {
            obj.insert(key.to_string(), value);
        }
        // The entry existed but was not a JSON object (only reachable from a
        // hand-built canonical value); replace it with a fresh single-key
        // object so the write is never silently lost.
        None => {
            let mut obj = serde_json::Map::new();
            obj.insert(key.to_string(), value);
            meta.insert(provider_id.to_string(), serde_json::Value::Object(obj));
        }
    }
}

/// The wire protocol an upstream provider speaks.
///
/// The four built-in variants are bidirectional — the SDK both serves them to
/// clients (via an
/// [`InboundAdapter`](crate::language_model::protocol::InboundAdapter)) and
/// calls them upstream (via an
/// [`OutboundAdapter`](crate::language_model::protocol::OutboundAdapter)).
///
/// [`Custom`](Self::Custom) is an extension point for *outbound-only*
/// platform providers (AWS Bedrock, Azure OpenAI, Vertex AI, …). Such a
/// provider lives in its own crate and registers an `OutboundAdapter` +
/// [`Transport`](crate::language_model::protocol::Transport) on the
/// executor's
/// [`OutboundDispatch`](crate::language_model::protocol::OutboundDispatch).
/// The name passed to `Custom` is the registration key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ApiProtocol {
    /// OpenAI-style Chat Completions (`POST /v1/chat/completions`).
    #[default]
    ChatCompletions,
    /// Anthropic-style Messages (`POST /v1/messages`).
    Messages,
    /// Google-style Generate Content (`POST …:generateContent`).
    GenerateContent,
    /// Responses.
    Responses,
    /// An externally-registered protocol identified by its registration name
    /// (e.g. `"bedrock-claude"`). The SDK does not serve `Custom` protocols
    /// inbound; they are outbound-only by design.
    Custom(String),
}

impl ApiProtocol {
    /// Stable string name for this protocol (`"chat_completions"`, `"messages"`, …, or
    /// the inner string for [`Custom`](Self::Custom)). Used as the wire-format
    /// representation in YAML config and as the registry key for outbound
    /// dispatch.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
            Self::GenerateContent => "generate_content",
            Self::Responses => "responses",
            Self::Custom(name) => name.as_str(),
        }
    }
}

impl std::fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ApiProtocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiProtocol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "chat_completions" => Self::ChatCompletions,
            "messages" => Self::Messages,
            "generate_content" => Self::GenerateContent,
            "responses" => Self::Responses,
            _ => Self::Custom(s),
        })
    }
}

/// A conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// System / developer instruction.
    System,
    /// End user.
    User,
    /// Model output.
    Assistant,
    /// Tool / function result.
    Tool,
}

/// One content block within a message. Ordered — mixed text + tool-call
/// sequences must preserve their order (v0 #416 regression).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text {
        /// The text body.
        text: String,
        /// Per-part namespaced provider metadata (V3 `providerMetadata` /
        /// `providerOptions`). On a text block this carries e.g. Anthropic
        /// `provider_metadata["anthropic"]["cacheControl"]` — the prompt-caching
        /// `{"type":"ephemeral"}` breakpoint that marks where the prompt cache
        /// boundary sits.
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// Model reasoning / thinking content (kept distinct so it is never
    /// silently dropped — v0 #454-1 regression).
    Reasoning {
        /// The reasoning text.
        text: String,
        /// Per-part namespaced provider metadata. Carries the reasoning trace's
        /// cryptographic continuity tokens so a multi-turn thinking conversation
        /// round-trips: Anthropic's thinking-block `signature` and the
        /// `redacted_thinking` marker under `provider_metadata["anthropic"]`, and
        /// Gemini's `thoughtSignature` under `provider_metadata["google"]`.
        ///
        /// The OpenAI Responses reasoning parse does **not** populate this slot:
        /// it lifts only the visible summary text and writes empty metadata, so
        /// OpenAI's `encrypted_content` reasoning continuity is not yet captured
        /// here (capturing it is future work, not an implied guarantee).
        /// <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A media / file part — image, audio, video, or document. Modelled à la the
    /// Vercel AI SDK `LanguageModelV3File`: a single media-typed part rather than
    /// a per-modality variant, identified by its IANA `media_type`.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-file.ts>
    File {
        /// IANA media type, e.g. `image/png`, `audio/mpeg`, `application/pdf`.
        media_type: String,
        /// The payload — inline base64 bytes or a URL the upstream fetches.
        data: DataContent,
        /// Original filename, when the provider supplies or requires it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Per-part namespaced provider metadata (V3 `providerMetadata` /
        /// `providerOptions`). Replaces the former ad-hoc `extra` map so there is
        /// one metadata mechanism across every part: an OpenAI image `detail`
        /// hint now rides at `provider_metadata["openai"]["detail"]`, and
        /// Anthropic block-level `cacheControl` at
        /// `provider_metadata["anthropic"]["cacheControl"]`.
        /// <https://platform.openai.com/docs/guides/vision>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A tool/function call requested by the model. Models the Vercel AI SDK
    /// `LanguageModelV3ToolCall` content part.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-tool-call.ts>
    ToolCall {
        /// Provider-assigned call id.
        id: String,
        /// Tool name.
        name: String,
        /// JSON-encoded arguments.
        arguments: String,
        /// Whether the tool ran **server-side at the provider** rather than
        /// being handed back to the client to execute — the V3
        /// `providerExecuted` flag. `false` (the default) is an ordinary client
        /// tool call (Chat Completions `tool_calls`, Anthropic `tool_use`,
        /// Gemini `functionCall`); `true` marks a provider-executed server tool
        /// (OpenAI Responses `web_search_call` / `code_interpreter_call` /
        /// `file_search_call`, Anthropic `server_tool_use`). The distinction is
        /// load-bearing: a provider-executed call must **not** be re-sent to the
        /// upstream as a client `function_call` on a follow-up turn (the
        /// provider already ran it), so render paths branch on this flag.
        /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-tool-call.ts>
        ///
        /// The sibling V3 `dynamic` flag (provider-executed MCP tools defined at
        /// runtime) is intentionally **not** modeled. It arises only from
        /// Anthropic `mcp_tool_use` and OpenAI Responses `mcp_call`, both of
        /// which carry a load-bearing server identifier (`server_name` /
        /// `server_label`) that this flat `ToolCall` has no slot for. Setting
        /// `dynamic` without also preserving that identifier would re-render an
        /// MCP block missing its server — worse than omitting it — and the
        /// Anthropic MCP connector is still beta-gated, not GA. So a faithful
        /// `dynamic` round-trip is deferred until the `ToolCall` shape grows a
        /// server-identifier slot, mirroring how the `execution-denied`
        /// tool-result variant is deferred above.
        /// <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        provider_executed: bool,
        /// Per-part namespaced provider metadata. On a `tool_use` block this
        /// carries the Anthropic block-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) — prompt caching
        /// applies to `tool_use` blocks just like text/image/document blocks.
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A tool/function result supplied back to the model. Models the Vercel AI
    /// SDK `LanguageModelV3ToolResultPart`: the result is a typed
    /// [`ToolResultOutput`] union (text / JSON / error / multimodal content)
    /// rather than a flat opaque string, so structure, the error flag, and
    /// multimodal results survive cross-protocol routing.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
    ToolResult {
        /// The call id this result answers.
        call_id: String,
        /// The tool's name, when the wire carries it. The V3 type makes this a
        /// required `string`, but that is faithful only to Gemini, whose
        /// `functionResponse` keys results by name. The OpenAI (Chat Completions
        /// and Responses) and Anthropic tool-result wires key purely by call id
        /// and never transmit the name, so it is genuinely absent there —
        /// modeling it as `Option` records that absence honestly. Fabricating a
        /// placeholder name to satisfy a required field would be worse: it would
        /// invent data the wire never carried and could collide with a real tool
        /// name on a downstream re-render. `None` is the correct value when the
        /// provider omits it; the field round-trips only where the wire supplies
        /// it (Gemini).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// The typed result body.
        output: ToolResultOutput,
        /// Per-part namespaced provider metadata. On a `tool_result` block this
        /// carries the Anthropic block-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) — prompt caching
        /// applies to `tool_result` blocks too, so a long tool output can mark a
        /// cache boundary.
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A citation / grounding source attached to the model's reply — the V3
    /// `LanguageModelV3Source` content part. Response-side only: an adapter's
    /// `parse_response` lifts a provider's native citation/annotation/grounding
    /// entries into these parts, and `render_response` re-attaches them to the
    /// provider's citation location. Sources never appear in a request, so
    /// request-side render paths skip this variant (documented at each site).
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-source.ts>
    Source {
        /// The citation source (URL or document).
        source: Source,
        /// Per-part namespaced provider metadata. Carries the Anthropic
        /// originating `tool_use_id` — the `server_tool_use` id that pairs with
        /// the `web_search_tool_result` block this source came from — under
        /// `provider_metadata["anthropic"]["toolUseId"]`, so the Anthropic render
        /// restores the *exact* pairing id rather than reusing one by position.
        /// <https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A human-in-the-loop **tool-approval request** — the V3
    /// `LanguageModelV3ToolApprovalRequest` content part. Emitted by a provider
    /// (assistant output) for a provider-executed tool call that needs explicit
    /// user approval before it runs; the matching [`Self::ToolApprovalResponse`]
    /// (keyed by [`approval_id`](Self::ToolApprovalRequest::approval_id)) carries
    /// the grant or denial back on the next turn.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-tool-approval-request.ts>
    ///
    /// The only wire that carries this handshake is OpenAI Responses, where it is
    /// the `mcp_approval_request` output item (`{id, type, server_label, name,
    /// arguments}`). On parse, `approval_id` is the item's `approval_request_id`
    /// (falling back to its `id`), and the MCP `server_label` / `name` /
    /// `arguments` ride in `provider_metadata["openai"]` so the render reproduces
    /// the exact `mcp_approval_request` item byte-for-byte. The other three wires
    /// (Anthropic / Generate Content / Chat Completions) have no approval-request
    /// item and skip this variant on render (documented at each site).
    /// <https://platform.openai.com/docs/api-reference/responses/object>
    ToolApprovalRequest {
        /// Approval id, referenced by the subsequent
        /// [`Self::ToolApprovalResponse`]. On Responses this is the
        /// `mcp_approval_request.approval_request_id` (falling back to its `id`).
        approval_id: String,
        /// The tool call this approval is for (V3 `toolCallId`). The Responses
        /// `mcp_approval_request` item does not transmit a separate tool-call id,
        /// so it is synthesized from `approval_id` (`approval:<approval_id>`) — a
        /// deterministic value, mirroring how the AI SDK generates one — and round
        /// trips stably on a same-protocol hop.
        tool_call_id: String,
        /// Per-part namespaced provider metadata. On a Responses
        /// `mcp_approval_request` this carries the MCP server identity under
        /// `provider_metadata["openai"]` (`serverLabel` / `name` / `arguments`) so
        /// the render restores the exact item; the canonical flat shape itself has
        /// no MCP-server slot.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A human-in-the-loop **tool-approval response** — the V3
    /// `LanguageModelV3ToolApprovalResponsePart`. Supplied back by the
    /// client/tool layer (a `tool`-role input part) to grant or deny a
    /// provider-executed tool call that emitted a [`Self::ToolApprovalRequest`],
    /// keyed by the shared [`approval_id`](Self::ToolApprovalResponse::approval_id).
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
    ///
    /// The only wire that carries it is OpenAI Responses, as the
    /// `mcp_approval_response` input item (`{type, approval_request_id,
    /// approve}`). On parse, `approval_id` is `approval_request_id` and `approved`
    /// is `approve`; on render this variant re-emits exactly that item. A denial
    /// (`approved == false`) additionally yields a paired
    /// [`ToolResultOutput::ExecutionDenied`] result carrying the approval id, so
    /// the structured denial survives in the canonical IR (see `parse_input` in
    /// the Responses adapter). The other three wires drop this part on render —
    /// the AI SDK's converters do the same (`continue`) — because they have no
    /// approval-response item (documented at each site).
    /// <https://platform.openai.com/docs/api-reference/responses/object>
    ToolApprovalResponse {
        /// The approval id this response answers — the
        /// [`Self::ToolApprovalRequest::approval_id`] it grants or denies. On
        /// Responses this is the `mcp_approval_response.approval_request_id`.
        approval_id: String,
        /// Whether the tool call is approved (`mcp_approval_response.approve`).
        approved: bool,
        /// Optional human-readable reason (V3 `reason`). No built-in wire carries
        /// it on the approval item — the Responses `mcp_approval_response` has no
        /// `reason` field — so it is `None` after any wire round-trip and rides
        /// only when a caller sets it on the canonical value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Per-part namespaced provider metadata (V3 `providerOptions`). Carries
        /// no Responses-native field today; present for parity with the other
        /// content parts and so a foreign provider's namespace survives a route.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
}

/// The result body of a [`Content::ToolResult`]. A faithful port of the Vercel
/// AI SDK `LanguageModelV3ToolResultOutput` tagged union: a tool result can be
/// plain text, a JSON value, an error (text or JSON), or a multimodal content
/// array. Adapters translate each variant to/from the upstream's native wire
/// shape and degrade losslessly when a provider can't express a variant.
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
///
/// The V3 union's sixth member, `execution-denied`
/// (`{ type: 'execution-denied', reason? }`), is modeled by
/// [`Self::ExecutionDenied`]. It arises from the human-in-the-loop tool-approval
/// flow ([`Content::ToolApprovalRequest`] / [`Content::ToolApprovalResponse`]):
/// when a user denies a provider-executed tool call, its result is a denial
/// rather than an ordinary output. Only OpenAI Responses carries the flow on the
/// wire; there the denial has two faithful renderings, both reproduced here
/// (`render_responses_tool_output`): a denial paired with its approval (the
/// approval id rides in `provider_metadata["openai"]["approvalId"]`) is **skipped**
/// on render — the `mcp_approval_response` already conveyed it — and an unpaired
/// denial degrades to a `function_call_output.output` string
/// (`reason ?? 'Tool call execution denied.'`) that re-parses as a [`Self::Text`].
/// On the other three wires it likewise degrades to that string via
/// [`Self::to_provider_string`].
/// <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultOutput {
    /// Plain-text output sent directly to the model.
    Text {
        /// The text body.
        value: String,
    },
    /// A structured JSON result.
    Json {
        /// The JSON value.
        value: serde_json::Value,
    },
    /// A textual error message (the V3 `error-text` output).
    ErrorText {
        /// The error text.
        value: String,
    },
    /// A structured JSON error (the V3 `error-json` output).
    ErrorJson {
        /// The error value.
        value: serde_json::Value,
    },
    /// A multimodal result: an ordered array of text / media parts.
    Content {
        /// The ordered content parts.
        value: Vec<ToolResultContentPart>,
    },
    /// A denied tool execution — the V3 `execution-denied` output. The result of
    /// a provider-executed tool call the user refused via the tool-approval flow
    /// ([`Content::ToolApprovalResponse`] with `approved == false`). `reason` is
    /// the optional human-readable explanation; when absent, renders fall back to
    /// `"Tool call execution denied."` (matching the AI SDK). On OpenAI Responses
    /// a denial paired with its approval is skipped on render (the
    /// `mcp_approval_response` conveys it); elsewhere it degrades to the reason
    /// string via [`Self::to_provider_string`].
    /// <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
    ExecutionDenied {
        /// Optional human-readable denial reason. `None` falls back to the
        /// default denial string on render.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl ToolResultOutput {
    /// Parse a provider tool-result body that carries no error flag and no
    /// typed shape (OpenAI Chat Completions `tool` content / Responses
    /// `function_call_output.output`). A JSON string maps to [`Self::Text`];
    /// any other JSON value (object, array, number, …) maps to [`Self::Json`],
    /// preserving structure the flat-string IR used to lose.
    pub fn from_untyped_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::String(s) => Self::Text { value: s.clone() },
            other => Self::Json {
                value: other.clone(),
            },
        }
    }

    /// Whether this output represents an error (the V3 `error-text` /
    /// `error-json` outputs). Lets an adapter set a provider's native error flag
    /// (e.g. Anthropic's `tool_result.is_error`).
    pub fn is_error(&self) -> bool {
        matches!(self, Self::ErrorText { .. } | Self::ErrorJson { .. })
    }

    /// Collapse this output to a single string for providers whose tool-result
    /// field is string-only (OpenAI Chat Completions / Responses). `Text` /
    /// `ErrorText` pass through verbatim; `Json` / `ErrorJson` stringify their
    /// value; `Content` concatenates its text parts (media parts have no string
    /// form on these wires and are dropped).
    pub fn to_provider_string(&self) -> String {
        match self {
            Self::Text { value } | Self::ErrorText { value } => value.clone(),
            Self::Json { value } | Self::ErrorJson { value } => value.to_string(),
            Self::Content { value } => value
                .iter()
                .filter_map(|p| match p {
                    ToolResultContentPart::Text { text } => Some(text.as_str()),
                    // Media bytes and provider file references have no string form
                    // on a string-only tool wire; both drop out of the collapse.
                    ToolResultContentPart::Media { .. } | ToolResultContentPart::FileId { .. } => {
                        None
                    }
                })
                .collect(),
            // A denied execution collapses to its reason, or the AI SDK's default
            // denial sentinel when none was given.
            // <https://github.com/vercel/ai/blob/main/packages/openai/src/responses/convert-to-openai-responses-input.ts>
            Self::ExecutionDenied { reason } => reason
                .clone()
                .unwrap_or_else(|| "Tool call execution denied.".to_string()),
        }
    }
}

/// One part of a [`ToolResultOutput::Content`] multimodal tool result. Mirrors
/// the V3 `content` output's element union. That union names eight element kinds
/// (`text`, `file-data`, `file-url`, `file-id`, `image-data`, `image-url`,
/// `image-file-id`, `custom`); they collapse onto the payload forms a
/// faithful-passthrough router actually carries:
/// - `text` → [`Self::Text`].
/// - `file-data` / `file-url` / `image-data` / `image-url` → [`Self::Media`],
///   whose [`DataContent`] holds inline base64 bytes or a URL (the data/URL split
///   absorbs the `*-data` vs `*-url` distinction; the IANA `media_type` absorbs
///   the file-vs-image distinction).
/// - `file-id` / `image-file-id` → [`Self::FileId`], a provider-side uploaded-file
///   reference (e.g. an OpenAI `file_id`) carried instead of inline bytes/URL.
/// - `custom` (`{ type: 'custom', providerOptions? }`) is dropped: it has no
///   transportable payload of its own — only opaque, provider-scoped
///   `providerOptions` with no cross-provider meaning — and the reference
///   conversions likewise drop any unrecognised content part, so omitting it is
///   lossless for a faithful-passthrough router.
///
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentPart {
    /// A text fragment.
    Text {
        /// The text body.
        text: String,
    },
    /// A media fragment — image, audio, etc., keyed by IANA media type.
    Media {
        /// IANA media type, e.g. `image/png`.
        media_type: String,
        /// The payload — inline base64 bytes or a URL.
        data: DataContent,
    },
    /// A provider-side uploaded-file reference (the V3 `file-id` /
    /// `image-file-id` content kinds): the bytes live with the provider and the
    /// tool result carries only the opaque id. On the OpenAI Responses wire this
    /// is an `input_image` / `input_file` part whose payload is `file_id` rather
    /// than `image_url` / `file_data`.
    /// <https://platform.openai.com/docs/api-reference/responses/input-item-list>
    FileId {
        /// IANA media type when known. Selects the image-vs-file rendering
        /// (`image/*` → `input_image`, otherwise `input_file`); `None` when the
        /// wire part gives no type hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// The provider's file identifier.
        id: String,
    },
}

/// The payload of a [`Content::File`] part. The Vercel AI SDK models this as
/// `Uint8Array | string | URL`; a faithful-passthrough router never decodes the
/// bytes, so we keep only the two wire forms every provider accepts: an inline
/// base64 blob or a URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataContent {
    /// Base64-encoded inline bytes (no `data:` URI prefix).
    Base64 {
        /// The base64 payload.
        data: String,
    },
    /// A URL the upstream fetches directly.
    Url {
        /// The resource URL.
        url: String,
    },
}

impl DataContent {
    /// Render as a single URL string: inline base64 bytes become a `data:` URI;
    /// a URL passes through unchanged. For wire forms that carry media in one
    /// URL field (e.g. OpenAI `image_url.url`).
    pub fn to_url(&self, media_type: &str) -> String {
        match self {
            DataContent::Base64 { data } => format!("data:{media_type};base64,{data}"),
            DataContent::Url { url } => url.clone(),
        }
    }

    /// Parse a URL-or-`data:`-URI into an optional embedded media type and the
    /// payload. `data:<mt>;base64,<payload>` yields `(Some(mt), Base64)`; any
    /// other string becomes `(None, Url)`.
    pub fn from_url(value: &str) -> (Option<String>, DataContent) {
        if let Some(rest) = value.strip_prefix("data:")
            && let Some((meta, payload)) = rest.split_once(',')
            && meta.contains(";base64")
        {
            let media_type = meta
                .split(';')
                .next()
                .filter(|m| !m.is_empty())
                .map(str::to_string);
            return (
                media_type,
                DataContent::Base64 {
                    data: payload.to_string(),
                },
            );
        }
        (
            None,
            DataContent::Url {
                url: value.to_string(),
            },
        )
    }
}

/// A citation / grounding source attached to the model's reply — a faithful
/// port of the Vercel AI SDK `LanguageModelV3Source` tagged union. A source is
/// **response-side metadata**: unlike text or media it is never a free-standing
/// part on any request wire. Providers carry it as message/response
/// *annotations* (OpenAI `url_citation`), text-block *citations* / a
/// `web_search_tool_result` block (Anthropic), or *grounding* metadata (Gemini
/// `groundingChunks`); an adapter's `parse_response` lifts those out into
/// [`Content::Source`] parts, and `render_response` re-attaches them at the
/// provider's native citation location (never per-part on a request).
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-source.ts>
///
/// V3 requires `id` on both variants, but providers rarely transmit a citation
/// id (OpenAI / Gemini / Anthropic web-search results carry none). Adapters
/// therefore **synthesize** a stable id from the source's URL and its index in
/// the response (see `synthesize_source_id` in each adapter) so the required
/// field is always populated without inventing identity the wire never carried.
///
/// The V3 per-source `providerMetadata` (e.g. Anthropic `cited_text` /
/// `encrypted_index`, a document citation's page/char ranges) is intentionally
/// **not** modeled here yet — it is deferred to the separate provider-metadata
/// task that adds a metadata slot uniformly across the content parts. Its
/// absence is why the text-to-source linkage (`cited_text` and char/page
/// offsets) cannot cross faithfully today; that loss is inherent to the current
/// parity target, not to any one adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source_type", rename_all = "snake_case")]
pub enum Source {
    /// A URL citation — the V3 `sourceType: 'url'` source. Maps to OpenAI
    /// `url_citation` annotations, Anthropic `web_search_result` /
    /// `web_search_result_location` citations, and Gemini web `groundingChunks`.
    Url {
        /// Stable source id. Synthesized from `url` + response index where the
        /// provider transmits none.
        id: String,
        /// The cited web address.
        url: String,
        /// Human-readable title of the cited page, when the provider supplies
        /// one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A document citation — the V3 `sourceType: 'document'` source. Maps to
    /// OpenAI Responses `file_citation` / `container_file_citation` /
    /// `file_path` annotations and Anthropic `page_location` / `char_location`
    /// document citations. `title` is required by V3; `media_type` identifies
    /// the cited document's IANA type.
    Document {
        /// Stable source id. Synthesized where the provider transmits none.
        id: String,
        /// IANA media type of the cited document, e.g. `text/plain` or
        /// `application/pdf`.
        media_type: String,
        /// Document title (required by V3). Adapters fall back to the filename
        /// or file id when the provider gives no separate title.
        title: String,
        /// Original filename, when the provider supplies one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
}

impl Source {
    /// Synthesize a stable [`Source`] id from a citation's URL and its index in
    /// the response. V3 requires an `id`, but the OpenAI / Anthropic web-search
    /// / Gemini grounding wires rarely carry one; this derives a deterministic,
    /// collision-resistant id (`url#<index>`) so a same-protocol round-trip
    /// reproduces a stable identity without fabricating provider-side identity.
    pub fn synthesize_id(url: &str, index: usize) -> String {
        format!("{url}#{index}")
    }
}

/// A single message in the conversation.
///
/// There is intentionally **no** message-level `provider_metadata` slot: no
/// request wire carries metadata on a message *separate from its content
/// blocks*. Anthropic prompt caching is expressed per content block (text /
/// image / document / `tool_use` / `tool_result`), and message-level
/// `cache_control` is not a distinct wire field — the Vercel AI SDK folds a
/// message's `cacheControl` onto its **last** content block rather than emitting
/// a sibling of `role`/`content` (which the Anthropic API rejects). Block-level
/// [`Content`] metadata therefore covers every caching breakpoint, so a
/// per-message slot would be unconstructed dead weight.
/// <https://github.com/vercel/ai/blob/main/packages/anthropic/src/convert-to-anthropic-prompt.ts>
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// The speaker role.
    pub role: Role,
    /// Ordered content blocks.
    pub content: Vec<Content>,
}

impl Message {
    /// Build a plain-text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::Text {
                text: text.into(),
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }
}

/// A tool the model may call — a faithful port of the Vercel AI SDK
/// `LanguageModelV3FunctionTool | LanguageModelV3ProviderTool` union.
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-function-tool.ts>
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-provider-tool.ts>
///
/// Like [`ToolChoice`] and [`ResponseFormat`], each inbound adapter promotes a
/// provider-native tool entry into this typed slot at parse time, and each
/// outbound adapter renders it back into the upstream's native shape. The two
/// variants behave very differently across protocols:
///
/// - [`Self::Function`] is portable: every protocol has a function/tool slot,
///   so a function tool round-trips across all four wires. Only the optional
///   `strict` flag is protocol-specific (Chat Completions / Responses honor it;
///   Anthropic and Gemini have no equivalent and drop it — documented at those
///   render sites).
/// - [`Self::ProviderDefined`] is **not** portable. V3 namespaces these tools by
///   provider precisely because their `args` schema is provider-specific and has
///   no cross-provider equivalent (an OpenAI `web_search_preview` is not an
///   Anthropic `web_search_20250305`). On a **same-protocol** round-trip the
///   native shape is reproduced exactly (`id` + `args` preserved). On a
///   **cross-protocol** route the tool is preserved **verbatim** in its source
///   provider's native shape and splatted into the target request so the
///   upstream — not bitrouter — decides what to do with it; bitrouter never
///   silently drops it and never invents a lossy "equivalent" (the same
///   faithful-passthrough rule as [`ToolChoice::Other`]). See
///   [`provider_defined_native`](crate::language_model::protocol) for the shared
///   reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    /// A client-side function tool: `{name, description?, parameters, strict?}`.
    /// `parameters` is V3's `inputSchema` (a JSON Schema). `strict` is V3's
    /// top-level strict-mode flag — captured from the wire where present so it
    /// is not lost across the canonical boundary.
    Function {
        /// Tool name. Unique within the request.
        name: String,
        /// Human description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// JSON Schema for the tool's parameters (V3 `inputSchema`).
        parameters: serde_json::Value,
        /// V3 strict-mode flag. Chat Completions / Responses honor it; Anthropic
        /// and Gemini have no equivalent and drop it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// Per-part namespaced provider metadata. On a tool this carries the
        /// Anthropic tool-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) — placing
        /// `cache_control` on the last tool definition caches the whole tools
        /// array, a common prompt-caching pattern for large tool catalogs.
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A provider-defined (server-side) tool: `{id, name, args}`, mirroring V3's
    /// `LanguageModelV3ProviderTool`. `id` is provider-namespaced as
    /// `<provider-id>.<tool-name>` (e.g. `openai.web_search_preview`,
    /// `anthropic.web_search_20250305`, `google.google_search`). `args` is the
    /// provider-specific configuration object for the tool, preserved verbatim.
    ProviderDefined {
        /// Provider-namespaced id, `<provider-id>.<tool-name>`.
        id: String,
        /// Tool name. Unique within the request.
        name: String,
        /// Provider-specific configuration arguments, preserved verbatim.
        args: serde_json::Value,
        /// Per-part namespaced provider metadata. Carries the Anthropic
        /// tool-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) for a server tool,
        /// mirroring [`Self::Function`].
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
}

impl Tool {
    /// The tool's name — present on both variants. Used where a feature needs to
    /// reference a tool by name without caring whether it is a client function
    /// tool or a provider-defined one (e.g. policy tool-access checks).
    pub fn name(&self) -> &str {
        match self {
            Self::Function { name, .. } | Self::ProviderDefined { name, .. } => name,
        }
    }
}

/// How the model must treat the available [`Tool`]s on this request — a faithful
/// port of the Vercel AI SDK `LanguageModelV3ToolChoice` tagged union.
/// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-tool-choice.ts>
///
/// Each protocol expresses tool choice with a different wire shape (Chat
/// Completions `"auto"|"none"|"required"` or `{type:"function",…}`; Anthropic
/// `{type:"auto"|"any"|"tool"|"none"}`; Responses `"auto"|"none"|"required"` or
/// `{type:"function",name}`; Gemini `functionCallingConfig.mode`). Each inbound
/// adapter promotes its native field into this typed slot at parse time and
/// **removes it from the raw `extra`** so it is not double-written; each outbound
/// adapter renders it back into the upstream's native shape. Promoting it makes
/// cross-protocol routing correct: without it a raw provider-shaped `tool_choice`
/// (e.g. an OpenAI `{type:"function",…}`) would be splatted verbatim into a
/// different provider's request and be silently ignored or rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool (V3 `auto`). The default when
    /// tools are present.
    Auto,
    /// The model must not call any tool (V3 `none`).
    None,
    /// The model must call one of the available tools, but which one is its
    /// choice (V3 `required`).
    Required,
    /// The model must call this specific tool by name (V3 `tool`).
    Tool {
        /// The tool name the model is forced to call.
        name: String,
    },
    /// A provider-specific tool-choice shape that does not map onto any of the
    /// four V3 variants (e.g. Gemini's `mode: "ANY"` paired with
    /// `allowedFunctionNames`, or an OpenAI `allowed_tools` constraint),
    /// preserved verbatim so an exotic choice is never lost on a same-protocol
    /// round-trip. Carries the raw provider-native JSON; an adapter that cannot
    /// express it on its own wire degrades it (documented at each render site).
    Other {
        /// The provider-native `tool_choice` value, untouched.
        value: serde_json::Value,
    },
}

/// Constraint on the shape of the model's response.
///
/// Today the only variant is [`Self::JsonSchema`]; future variants (`json_object`,
/// `text`, `regex`) can be added without breaking existing call sites.
///
/// Each inbound adapter promotes the provider-native field into this typed
/// slot at `parse_request` time (e.g. Chat Completions' `response_format`,
/// Messages' `output_config.format`, Generate Content's `generationConfig.responseSchema`).
/// Each outbound adapter renders it back into the upstream's native shape on
/// `render_request`. Cross-protocol routing therefore works automatically:
/// a Chat Completions client asking for `json_schema` against a Messages upstream
/// emits `output_config.format`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Constrain output to a JSON Schema.
    JsonSchema {
        /// Schema name. Required by Chat Completions / Responses; ignored by
        /// Messages and Generate Content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Strict-mode flag. Chat Completions / Responses only; Messages and Generate Content are always strict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// An optional inference feature a request may require and a provider may
/// advertise. Capabilities are API-agnostic: the same capability maps to a
/// different wire parameter in each protocol (structured outputs is Chat
/// Completions' `response_format`, Messages' `output_config.format`, Generate
/// Content's `responseSchema`, Responses' `text.format`). The serde
/// representation (e.g. `structured_outputs`) is the stable token used to match
/// a request's needs against a provider's advertised capabilities.
///
/// A capability-aware [`RoutingTable`](crate::language_model::routing::RoutingTable)
/// can read the set a request requires ([`Prompt::required_capabilities`]) and
/// restrict the chain to providers advertising all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// JSON-Schema-constrained output — [`ResponseFormat::JsonSchema`].
    StructuredOutputs,
    /// Tool / function calling — the request supplies a non-empty tool list.
    Tools,
    /// Extended reasoning / thinking — the request sets a reasoning-effort hint.
    Reasoning,
    /// Provider-side web search / browsing.
    WebSearch,
    /// Token log-probabilities in the response.
    Logprobs,
    /// Image input (vision) — a message carries an `image/*` file part.
    ImageInput,
    /// Audio input — a message carries an `audio/*` file part.
    AudioInput,
    /// Video input — a message carries a `video/*` file part.
    VideoInput,
    /// Document / file input — a message carries a non-image/audio/video file
    /// part (e.g. `application/pdf`).
    FileInput,
    /// Image output (generation) — the request asks for an `image` output
    /// modality.
    ImageOutput,
    /// Audio output (speech) — the request asks for an `audio` output modality.
    AudioOutput,
}

impl Capability {
    /// The stable token string (e.g. `"structured_outputs"`), equal to this
    /// enum's serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StructuredOutputs => "structured_outputs",
            Self::Tools => "tools",
            Self::Reasoning => "reasoning",
            Self::WebSearch => "web_search",
            Self::Logprobs => "logprobs",
            Self::ImageInput => "image_input",
            Self::AudioInput => "audio_input",
            Self::VideoInput => "video_input",
            Self::FileInput => "file_input",
            Self::ImageOutput => "image_output",
            Self::AudioOutput => "audio_output",
        }
    }

    /// The input-modality capability implied by a file part's IANA media type;
    /// everything that is not image/audio/video is treated as a document.
    fn from_input_media_type(media_type: &str) -> Capability {
        match media_type.split('/').next() {
            Some("image") => Capability::ImageInput,
            Some("audio") => Capability::AudioInput,
            Some("video") => Capability::VideoInput,
            _ => Capability::FileInput,
        }
    }

    /// The output-modality capability implied by a requested response modality;
    /// `Text` implies no extra capability.
    fn from_output_modality(modality: Modality) -> Option<Capability> {
        match modality {
            Modality::Image => Some(Capability::ImageOutput),
            Modality::Audio => Some(Capability::AudioOutput),
            Modality::Text => None,
        }
    }
}

/// A content modality. Used for the requested output modalities
/// ([`GenerationParams::response_modalities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Image.
    Image,
    /// Audio.
    Audio,
}

/// Sampling / generation parameters, carried verbatim where possible.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Maximum tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reasoning effort hint (`low` / `medium` / `high`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Requested output modalities (OpenAI `modalities` / Gemini
    /// `responseModalities`). Empty means text-only (the default). Drives
    /// `image_output` / `audio_output` capability detection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_modalities: Vec<Modality>,
    /// How the model must treat the available tools (V3 `toolChoice`). Each
    /// inbound adapter promotes its native `tool_choice` field into this typed
    /// slot and removes it from `extra`; each outbound adapter renders it back
    /// into the upstream's native shape, so it routes correctly across
    /// protocols instead of leaking a raw provider-shaped value through `extra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Any provider-specific extras passed through untouched.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The canonical internal request representation. Inbound protocol adapters
/// parse into this; outbound adapters render from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    /// Requested model name (post preset/variant stripping).
    pub model: String,
    /// System instruction, if any. Distinct from the message list because some
    /// providers (Anthropic) carry it out-of-band.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Namespaced provider metadata for the system instruction (V3
    /// `providerOptions` on the system message). Kept parallel to [`Self::system`]
    /// — rather than folded into a `SystemPrompt` shape — so the common
    /// `system: Option<String>` reads stay untouched across every adapter.
    ///
    /// Its load-bearing use is Anthropic system-prompt **prompt caching**: a
    /// system block may carry `cache_control` (`{"type":"ephemeral"}`), and the
    /// system prefix is the highest-value, most common cache breakpoint. It rides
    /// here under `system_provider_metadata["anthropic"]["cacheControl"]` so the
    /// Messages adapter can re-render the system as a cached
    /// `[{type:"text", text, cache_control}]` block instead of a bare string,
    /// which a plain `Option<String>` would otherwise flatten away.
    /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub system_provider_metadata: ProviderMetadata,
    /// Conversation messages, in order.
    pub messages: Vec<Message>,
    /// Tools available to the model.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<Tool>,
    /// Generation parameters.
    #[serde(default)]
    pub params: GenerationParams,
    /// Constraint on the model's response shape (e.g. a JSON Schema). Inbound
    /// adapters promote the provider-native field into this slot; outbound
    /// adapters render it back natively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Whether the caller requested a streaming response.
    pub stream: bool,
}

impl Prompt {
    /// The [`Capability`]s this request requires, derived from which optional
    /// features it actually uses. A capability-aware routing table can use this
    /// to restrict the fallback chain to providers that advertise all of these,
    /// instead of silently degrading the request.
    ///
    /// Detected from the canonical [`Prompt`]: `structured_outputs` (a
    /// `response_format`), `tools` (a non-empty tool list), `reasoning` (a
    /// reasoning-effort hint), the input modalities of any [`Content::File`]
    /// parts (`image_input` / `audio_input` / `video_input` / `file_input`), and
    /// the requested output modalities (`image_output` / `audio_output`).
    /// `web_search` and `logprobs` ride [`GenerationParams::extra`] and are not
    /// expressible here, so they stay catalog-only and never gate routing.
    pub fn required_capabilities(&self) -> Vec<Capability> {
        let mut caps = Vec::new();
        if self.response_format.is_some() {
            caps.push(Capability::StructuredOutputs);
        }
        if !self.tools.is_empty() {
            caps.push(Capability::Tools);
        }
        if self.params.reasoning_effort.is_some() {
            caps.push(Capability::Reasoning);
        }
        // Input modalities: each file part contributes the capability implied by
        // its media type, deduplicated (one `image_input` for several images).
        for content in self.messages.iter().flat_map(|m| &m.content) {
            if let Content::File { media_type, .. } = content {
                let cap = Capability::from_input_media_type(media_type);
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
        }
        // Output modalities: from the requested response modalities.
        for modality in &self.params.response_modalities {
            if let Some(cap) = Capability::from_output_modality(*modality)
                && !caps.contains(&cap)
            {
                caps.push(cap);
            }
        }
        caps
    }
}

/// Token usage counts. Counts use `0` (not `null`) for "known to be zero";
/// missing usage is represented by `Option<Usage>` being `None` upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt / input tokens.
    pub prompt_tokens: u64,
    /// Completion / output tokens.
    pub completion_tokens: u64,
    /// Reasoning tokens (subset of `completion_tokens` on most providers).
    pub reasoning_tokens: u64,
    /// Cache-read input tokens — already-cached prompt content that the
    /// provider served from cache. Subset of `prompt_tokens`. Maps to
    /// Messages' `usage.cache_read_input_tokens`
    /// (<https://docs.anthropic.com/en/api/messages>) and to Chat Completions'
    /// `usage.prompt_tokens_details.cached_tokens`. Default 0 when the
    /// upstream reports no cache stats.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    /// Cache-write input tokens — prompt content written to the cache this
    /// turn. Subset of `prompt_tokens`. Maps to Messages'
    /// `usage.cache_creation_input_tokens`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_write_tokens: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl Usage {
    /// Total tokens (prompt + completion).
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Why generation stopped.
///
/// `Other` and `Error` are escape valves: a finish reason the canonical set
/// doesn't model (kept verbatim for observability), or a mid-stream upstream
/// failure surfaced through the canonical IR rather than abruptly aborting
/// the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop.
    Stop,
    /// Hit the max-tokens limit.
    Length,
    /// Stopped to emit a tool call.
    ToolCalls,
    /// Stopped by a content filter / guardrail.
    ContentFilter,
    /// An upstream-provided reason this enum doesn't model — kept verbatim
    /// (e.g. Anthropic's `pause_turn`, an unrecognised OpenAI value).
    Other(String),
    /// Generation aborted mid-stream because upstream returned a stream error
    /// event. Carries the upstream message for observability and for
    /// outbound encoders that want to emit a terminal error frame.
    Error(String),
}

/// Structured detail about why generation stopped. Today this is populated only
/// for refusals — Anthropic returns a `stop_details` object alongside
/// `stop_reason: "refusal"` (surfaced canonically as
/// [`FinishReason::ContentFilter`]) carrying the policy `category` and a
/// human-readable `explanation`. Both are optional: a refusal may map to no
/// named category, and the explanation can be absent or non-stable.
///
/// <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons#refusal-categories>
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopDetails {
    /// Policy category that triggered the stop (e.g. `"cyber"`, `"bio"`).
    /// `None` when the stop maps to no named category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Human-readable description of the stop. Not guaranteed stable — surface
    /// it, don't branch on it. `None` when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// A complete non-streaming generation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateResult {
    /// Ordered content blocks of the model's reply.
    pub content: Vec<Content>,
    /// Token usage, if the provider reported it.
    pub usage: Option<Usage>,
    /// Finish reason, if the provider reported it.
    pub finish_reason: Option<FinishReason>,
    /// Provider-assigned response id (e.g. Chat Completions `chatcmpl-...`,
    /// Messages `msg_...`, Responses `resp_...`, Generate Content
    /// `responseId`). Carried so observability can stamp it onto the OTel
    /// `gen_ai.response.id` semconv attribute and operators can correlate
    /// against the upstream provider's own logs. `None` when the provider
    /// did not surface one.
    ///
    /// Spec: <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Structured stop detail (e.g. a refusal category) when the provider
    /// supplies one. Maps to Anthropic's `stop_details`. `None` for providers
    /// or responses that carry no detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    /// Result-level namespaced provider metadata (V3 `providerMetadata` on the
    /// generation result). Carries response-level provider nuances that have no
    /// dedicated canonical field: OpenAI `system_fingerprint`
    /// (`provider_metadata["openai"]["systemFingerprint"]`) and Gemini
    /// `modelVersion` (`provider_metadata["google"]["modelVersion"]`).
    /// <https://platform.openai.com/docs/api-reference/chat/object>
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_metadata: ProviderMetadata,
}

/// One part of a streaming response, in canonical internal form. `StreamHook`
/// operates on a `Stream<Item = StreamPart>` before outbound protocol
/// conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamPart {
    /// An incremental chunk of assistant text.
    TextDelta {
        /// The text fragment.
        text: String,
    },
    /// An incremental chunk of reasoning / thinking text.
    ReasoningDelta {
        /// The reasoning fragment.
        text: String,
    },
    /// An incremental chunk of a tool call's arguments.
    ToolCallDelta {
        /// The call id this delta belongs to.
        id: String,
        /// Tool name (sent once, on the first delta).
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Arguments fragment.
        arguments: String,
    },
    /// A complete generated file (e.g. an image), emitted whole — matching the
    /// Vercel AI SDK `LanguageModelV3` stream `file` part, where files arrive as
    /// one part rather than chunked deltas.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-file.ts>
    File {
        /// IANA media type, e.g. `image/png`.
        media_type: String,
        /// The file payload — inline base64 bytes or a URL.
        data: DataContent,
    },
    /// A citation / grounding source, emitted whole — the V3 stream `source`
    /// part. Citations stream as one complete part (never chunked deltas):
    /// Gemini emits the candidate's `groundingChunks` per chunk and OpenAI Chat
    /// streams `delta.annotations`, both decoded here; the client-side encoders
    /// re-attach it to the protocol's native citation location.
    /// <https://github.com/vercel/ai/blob/main/packages/provider/src/language-model/v3/language-model-v3-source.ts>
    Source {
        /// The citation source (URL or document).
        source: Source,
    },
    /// A usage report. May arrive mid-stream (per-checkpoint) or only at the end.
    Usage {
        /// The usage counts.
        usage: Usage,
    },
    /// The provider-assigned response id, surfaced once near the start of
    /// the stream — Chat Completions' top-level `id`, Anthropic's
    /// `message_start.message.id`, Generate Content's `responseId`. (Responses
    /// carries its id on the terminal [`Self::ResponseCompleted`] instead.)
    ///
    /// Not client-facing: outbound encoders drop it (the client gets an id
    /// the inbound encoder generates). Observability uses it to stamp the
    /// GenAI semconv `gen_ai.response.id` attribute on the trace so the
    /// streaming path matches the non-streaming one.
    ///
    /// Spec: <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>
    ResponseStarted {
        /// The provider's response id.
        id: String,
    },
    /// The terminal part: generation finished.
    Finish {
        /// Why generation stopped.
        reason: FinishReason,
    },
    /// Terminal lifecycle part for Responses — preserves the response id
    /// and status that a bare [`StreamPart::Finish`] would lose.
    /// Only the Responses decoder emits this; the other protocols' encoders
    /// treat it as a terminal part equivalent to `Finish`.
    ResponseCompleted {
        /// The provider's response id.
        id: String,
        /// The terminal status (`completed` / `incomplete` / `failed`).
        status: String,
        /// Final usage, if the provider reported it.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

impl StreamPart {
    /// Whether this part terminates the stream (`Finish` or `ResponseCompleted`).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. }
        )
    }
}

/// The result of executing one routing target — the upstream response plus
/// timing. Written into `PipelineContext` after Stage 3.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// The provider id that served the request.
    pub provider_id: String,
    /// The model/service id at that provider.
    pub model_id: String,
    /// Which account of a multi-account provider served the request —
    /// `None` for a single-credential provider. Reflects any failover
    /// hop, so it can differ from the chain's primary account.
    pub account_label: Option<String>,
    /// The generation result.
    pub result: GenerateResult,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
}

/// How an upstream's credential is presented on the wire.
///
/// Consulted by transports that support more than one credential scheme.
/// Today that is only the Messages transport, which sends the key as either
/// `x-api-key` (Anthropic's native scheme) or `Authorization: Bearer`. The
/// Chat Completions transport (always `Authorization: Bearer`) and Generate
/// Content transport (always `x-goog-api-key`) have a single fixed scheme and
/// ignore this.
///
/// Exactly one scheme is ever sent — never both: the Anthropic API rejects a
/// request that carries `x-api-key` and `Authorization` together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum AuthScheme {
    /// `x-api-key: <key>` — Anthropic's native scheme; the default.
    #[default]
    #[serde(rename = "x-api-key")]
    XApiKey,
    /// `Authorization: Bearer <key>`.
    #[serde(rename = "bearer")]
    Bearer,
}

/// One hop in a fallback chain: a concrete provider + model + connection info.
///
/// The `Debug` impl redacts `api_key` and `api_key_override` (v0 audit S9):
/// any `tracing::error!(?target, ...)` call by a future contributor would
/// otherwise leak the upstream credential into structured logs.
#[derive(Clone)]
pub struct RoutingTarget {
    /// Provider id (config key).
    pub provider_name: String,
    /// Model / service id at the provider (may differ from the request model).
    pub service_id: String,
    /// Upstream API base URL.
    pub api_base: String,
    /// Upstream API key.
    pub api_key: String,
    /// The wire protocol this target speaks.
    pub api_protocol: ApiProtocol,
    /// Which account of a multi-account provider this target came from
    /// — `None` for a single-credential provider. Surfaced in the
    /// request log so an operator can see which subscription served a
    /// request; carries no routing behaviour itself.
    pub account_label: Option<String>,
    /// Per-request key override. Set by a `RouteHook` that wants to
    /// substitute the caller's own provider key (e.g. BYOK) for this hop.
    /// The SDK itself is opinion-free about whether or how such a hook
    /// exists; it just honours the override when set.
    pub api_key_override: Option<String>,
    /// Per-request api-base override, paired with `api_key_override`.
    pub api_base_override: Option<String>,
    /// How the credential is presented to this target. Consulted by
    /// transports that support more than one scheme — today only the Messages
    /// transport (`x-api-key` vs `Authorization: Bearer`); others ignore it.
    /// Defaults to [`AuthScheme::XApiKey`].
    pub auth_scheme: AuthScheme,
}

impl std::fmt::Debug for RoutingTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingTarget")
            .field("provider_name", &self.provider_name)
            .field("service_id", &self.service_id)
            .field("api_base", &self.api_base)
            .field("api_key", &redacted(&self.api_key))
            .field("api_protocol", &self.api_protocol)
            .field("account_label", &self.account_label)
            .field(
                "api_key_override",
                &self.api_key_override.as_deref().map(redacted),
            )
            .field("api_base_override", &self.api_base_override)
            .field("auth_scheme", &self.auth_scheme)
            .finish()
    }
}

fn redacted(s: &str) -> &'static str {
    if s.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

impl RoutingTarget {
    /// The effective API key (override wins).
    pub fn effective_api_key(&self) -> &str {
        self.api_key_override.as_deref().unwrap_or(&self.api_key)
    }

    /// The effective API base (override wins).
    pub fn effective_api_base(&self) -> &str {
        self.api_base_override.as_deref().unwrap_or(&self.api_base)
    }
}

/// Input to the `language_model` pipeline. Built by an inbound protocol adapter.
#[derive(Debug, Clone)]
pub struct PipelineRequest {
    /// Unique request id (generated if the inbound adapter has none).
    pub request_id: String,
    /// The raw requested model string (may carry `@preset` / `:variant`).
    pub model: String,
    /// The authenticated (or synthesised) caller.
    pub caller: CallerContext,
    /// Inbound HTTP headers.
    pub headers: http::HeaderMap,
    /// The canonical request body.
    pub prompt: Prompt,
}

impl PipelineRequest {
    /// Build a request with a fresh uuid request id.
    pub fn new(model: impl Into<String>, caller: CallerContext, prompt: Prompt) -> Self {
        let model = model.into();
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            model,
            caller,
            headers: http::HeaderMap::new(),
            prompt,
        }
    }
}

/// Output of a non-streaming pipeline run.
#[derive(Debug, Clone)]
pub struct PipelineResponse {
    /// The request id this answers.
    pub request_id: String,
    /// The generation result.
    pub result: GenerateResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_prompt() -> Prompt {
        Prompt {
            model: "m".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            stream: false,
        }
    }

    #[test]
    fn plain_request_requires_no_capabilities() {
        assert!(bare_prompt().required_capabilities().is_empty());
    }

    #[test]
    fn response_format_requires_structured_outputs() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        assert_eq!(
            p.required_capabilities(),
            vec![Capability::StructuredOutputs]
        );
    }

    #[test]
    fn tools_require_tools_capability() {
        let mut p = bare_prompt();
        p.tools = vec![Tool::Function {
            name: "get_weather".into(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
            strict: None,
            provider_metadata: Default::default(),
        }];
        assert_eq!(p.required_capabilities(), vec![Capability::Tools]);
    }

    #[test]
    fn reasoning_effort_requires_reasoning_capability() {
        let mut p = bare_prompt();
        p.params.reasoning_effort = Some("high".to_string());
        assert_eq!(p.required_capabilities(), vec![Capability::Reasoning]);
    }

    #[test]
    fn multiple_features_require_all_their_capabilities() {
        let mut p = bare_prompt();
        p.response_format = Some(ResponseFormat::JsonSchema {
            name: None,
            strict: None,
            schema: serde_json::json!({ "type": "object" }),
        });
        p.tools = vec![Tool::Function {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({}),
            strict: None,
            provider_metadata: Default::default(),
        }];
        p.params.reasoning_effort = Some("low".to_string());
        let caps = p.required_capabilities();
        assert!(caps.contains(&Capability::StructuredOutputs));
        assert!(caps.contains(&Capability::Tools));
        assert!(caps.contains(&Capability::Reasoning));
        assert_eq!(caps.len(), 3);
    }

    fn user_file_prompt(media_type: &str) -> Prompt {
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![Content::File {
                media_type: media_type.to_string(),
                data: DataContent::Base64 {
                    data: "AAAA".to_string(),
                },
                filename: None,
                provider_metadata: Default::default(),
            }],
        }];
        p
    }

    #[test]
    fn image_file_requires_image_input() {
        assert_eq!(
            user_file_prompt("image/png").required_capabilities(),
            vec![Capability::ImageInput]
        );
    }

    #[test]
    fn audio_file_requires_audio_input() {
        assert_eq!(
            user_file_prompt("audio/mpeg").required_capabilities(),
            vec![Capability::AudioInput]
        );
    }

    #[test]
    fn video_file_requires_video_input() {
        assert_eq!(
            user_file_prompt("video/mp4").required_capabilities(),
            vec![Capability::VideoInput]
        );
    }

    #[test]
    fn document_file_requires_file_input() {
        assert_eq!(
            user_file_prompt("application/pdf").required_capabilities(),
            vec![Capability::FileInput]
        );
    }

    #[test]
    fn repeated_image_parts_dedupe_to_one_image_input() {
        let img = || Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/a.png".to_string(),
            },
            filename: None,
            provider_metadata: Default::default(),
        };
        let mut p = bare_prompt();
        p.messages = vec![Message {
            role: Role::User,
            content: vec![img(), img()],
        }];
        assert_eq!(p.required_capabilities(), vec![Capability::ImageInput]);
    }

    #[test]
    fn response_modalities_require_output_capabilities() {
        let mut p = bare_prompt();
        p.params.response_modalities = vec![Modality::Text, Modality::Image, Modality::Audio];
        // Text implies no capability; image / audio each map to an output cap.
        assert_eq!(
            p.required_capabilities(),
            vec![Capability::ImageOutput, Capability::AudioOutput]
        );
    }

    #[test]
    fn file_content_serde_round_trips() {
        let original = Content::File {
            media_type: "image/png".to_string(),
            data: DataContent::Url {
                url: "https://example.invalid/y.png".to_string(),
            },
            filename: Some("y.png".to_string()),
            provider_metadata: Default::default(),
        };
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["type"], "file");
        assert_eq!(value["media_type"], "image/png");
        assert_eq!(value["data"]["kind"], "url");
        assert_eq!(value["data"]["url"], "https://example.invalid/y.png");
        let back: Content = serde_json::from_value(value).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn capability_token_matches_registry_vocabulary() {
        // These tokens must stay byte-identical to the registry Zod enum and
        // the cloud's imported `Capability` — they are the cross-repo contract.
        assert_eq!(Capability::StructuredOutputs.as_str(), "structured_outputs");
        assert_eq!(Capability::Tools.as_str(), "tools");
        assert_eq!(Capability::Reasoning.as_str(), "reasoning");
        assert_eq!(Capability::WebSearch.as_str(), "web_search");
        assert_eq!(Capability::Logprobs.as_str(), "logprobs");
        assert_eq!(Capability::ImageInput.as_str(), "image_input");
        assert_eq!(Capability::AudioInput.as_str(), "audio_input");
        assert_eq!(Capability::VideoInput.as_str(), "video_input");
        assert_eq!(Capability::FileInput.as_str(), "file_input");
        assert_eq!(Capability::ImageOutput.as_str(), "image_output");
        assert_eq!(Capability::AudioOutput.as_str(), "audio_output");
    }

    #[test]
    fn extra_only_features_are_not_auto_detected() {
        // web_search / logprobs ride `params.extra` and are advertise-only (not
        // gated) — a request carrying them must still require no capabilities.
        let mut p = bare_prompt();
        p.params
            .extra
            .insert("logprobs".into(), serde_json::json!(true));
        p.params
            .extra
            .insert("web_search_options".into(), serde_json::json!({}));
        assert!(p.required_capabilities().is_empty());
    }
}
