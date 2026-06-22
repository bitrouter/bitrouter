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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/shared/v3/shared-v3-provider-metadata.ts>
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

/// A wire protocol always (de)serializes as a string: one of the four known
/// values, or any other string for an externally-registered `Custom` protocol.
/// Hand-written because the `Custom(String)` variant means the value is an
/// open string set, not a closed enum.
impl schemars::JsonSchema for ApiProtocol {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ApiProtocol")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Wire protocol. Known values: `chat_completions`, \
                `messages`, `generate_content`, `responses`; any other string \
                names an externally-registered (outbound-only) custom protocol.",
            "examples": ["chat_completions", "messages", "generate_content", "responses"],
        })
    }
}

/// One or more wire protocols a `(provider, model)` can be served under, in
/// preference order. The list head is the *preferred* (default) outbound
/// protocol; protocol-native routing may instead pick whichever member matches
/// the inbound request's protocol, turning a lossy cross-protocol translation
/// into a faithful same-protocol round-trip.
///
/// Deserializes from **either** a bare protocol string or a sequence, so a
/// single-protocol provider and a multi-protocol one share one schema:
///
/// ```yaml
/// api_protocol:
///   - "*": ["chat_completions", "responses", "messages"]   # an ordered set
///   - "claude-*": messages                                  # still a bare string
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProtocolList(pub Vec<ApiProtocol>);

impl ProtocolList {
    /// The preferred (default) protocol — the list head.
    pub fn preferred(&self) -> Option<&ApiProtocol> {
        self.0.first()
    }

    /// Whether `protocol` is one of the supported protocols.
    pub fn contains(&self, protocol: &ApiProtocol) -> bool {
        self.0.contains(protocol)
    }

    /// The supported protocols as a slice, in preference order.
    pub fn as_slice(&self) -> &[ApiProtocol] {
        &self.0
    }

    /// Whether no protocol is listed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<ApiProtocol> for ProtocolList {
    fn from(p: ApiProtocol) -> Self {
        ProtocolList(vec![p])
    }
}

impl Serialize for ProtocolList {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

/// A `ProtocolList` deserializes from **either** a bare protocol string or an
/// array of them (see the type docs), so its schema is the union of the two.
impl schemars::JsonSchema for ProtocolList {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ProtocolList")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let one = generator.subschema_for::<ApiProtocol>();
        let many = generator.subschema_for::<Vec<ApiProtocol>>();
        schemars::json_schema!({
            "description": "One protocol (bare string) or an ordered set of them \
                (array); the head is the preferred outbound protocol.",
            "anyOf": [one, many],
        })
    }
}

impl<'de> Deserialize<'de> for ProtocolList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Accept a bare protocol string (`"messages"`) or a sequence
        // (`["messages", "chat_completions"]`). Both formats we parse from
        // (YAML, TOML) are self-describing, so the untagged dispatch is
        // unambiguous: a string deserializes `One`, a sequence `Many`.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(ApiProtocol),
            Many(Vec<ApiProtocol>),
        }
        Ok(match OneOrMany::deserialize(d)? {
            OneOrMany::One(p) => ProtocolList(vec![p]),
            OneOrMany::Many(v) => ProtocolList(v),
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-file.ts>
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-call.ts>
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
        /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-call.ts>
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        provider_executed: bool,
        /// The sibling V3 `dynamic` flag — a provider-executed tool defined at
        /// **runtime**, i.e. an MCP (Model Context Protocol) tool the provider
        /// runs on a remote server. `false` (the default) is an ordinary,
        /// statically-declared tool. `true` marks a call that arrived as an
        /// Anthropic `mcp_tool_use` block or an OpenAI Responses `mcp_call` item;
        /// such a call carries a load-bearing **server identifier** — Anthropic's
        /// `server_name` or OpenAI's `server_label` — which this flat shape has no
        /// core field for, so it rides in
        /// [`provider_metadata`](Self::ToolCall::provider_metadata) under the
        /// originating provider's namespace (`provider_metadata["anthropic"]`'s
        /// `serverName` / `type: "mcp-tool-use"`, or
        /// `provider_metadata["openai"]`). The render paths branch on this flag to
        /// re-emit the MCP-native block on the **same** wire (`mcp_tool_use` /
        /// `mcp_call`) and degrade it to a plain tool call on every other wire,
        /// where the server identity is provider-specific and is dropped.
        /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-call.ts>
        /// <https://platform.claude.com/docs/en/agents-and-tools/mcp-connector>
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dynamic: bool,
        /// Per-part namespaced provider metadata. On a `tool_use` block this
        /// carries the Anthropic block-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) — prompt caching
        /// applies to `tool_use` blocks just like text/image/document blocks. For
        /// a [`dynamic`](Self::ToolCall::dynamic) MCP call it ALSO carries the MCP
        /// server identity (Anthropic `serverName` + `type: "mcp-tool-use"`, or
        /// OpenAI `serverLabel`).
        /// <https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching>
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A tool/function result supplied back to the model. Models the Vercel AI
    /// SDK `LanguageModelV3ToolResultPart`: the result is a typed
    /// [`ToolResultOutput`] union (text / JSON / error / multimodal content)
    /// rather than a flat opaque string, so structure, the error flag, and
    /// multimodal results survive cross-protocol routing.
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
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
        /// The V3 `LanguageModelV3ToolResult` `dynamic` flag — set when this
        /// result answers a [`dynamic`](Self::ToolCall::dynamic) provider-executed
        /// MCP tool call, i.e. it arrived **inline** with its call as an Anthropic
        /// `mcp_tool_result` block or as the `output`/`error` carried on an OpenAI
        /// Responses `mcp_call` item. `false` (the default) is an ordinary tool
        /// result. The render paths use it to pair the result back with its MCP
        /// call: Anthropic re-emits an `mcp_tool_result` block (rather than a plain
        /// `tool_result`), and OpenAI recombines this result with its same-id
        /// dynamic [`ToolCall`](Self::ToolCall) into a single `mcp_call` output
        /// item. On any non-MCP wire the flag is simply dropped and the result
        /// degrades to that provider's ordinary tool-result shape.
        /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-result.ts>
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dynamic: bool,
        /// Per-part namespaced provider metadata. On a `tool_result` block this
        /// carries the Anthropic block-level `cacheControl` breakpoint
        /// (`provider_metadata["anthropic"]["cacheControl"]`) — prompt caching
        /// applies to `tool_result` blocks too, so a long tool output can mark a
        /// cache boundary. For an OpenAI Responses `mcp_call` result it also
        /// carries the originating item id under
        /// `provider_metadata["openai"]["itemId"]`, restored when recombining the
        /// inline `mcp_call`.
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-source.ts>
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-tool-approval-request.ts>
    ///
    /// The only wire that carries this handshake is OpenAI Responses, where it is
    /// the `mcp_approval_request` output item (`{id, type, server_label, name,
    /// arguments, approval_request_id?}`). On parse, `approval_id` is the item's
    /// `approval_request_id` (the correlation key, falling back to its `id`), and
    /// the MCP `server_label` / `name` / `arguments` ride in
    /// `provider_metadata["openai"]` so the render reproduces the exact
    /// `mcp_approval_request` item byte-for-byte. When the item carried a raw `id`
    /// *distinct* from that correlation key, the `id` is also preserved under
    /// `provider_metadata["openai"]["itemId"]` and restored on render, so the
    /// two-id form round-trips losslessly. The other three wires (Anthropic /
    /// Generate Content / Chat Completions) have no approval-request item and skip
    /// this variant on render (documented at each site).
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
        /// `provider_metadata["openai"]` (`serverLabel` / `name` / `arguments`),
        /// plus the raw item `id` as `itemId` when it differed from `approval_id`,
        /// so the render restores the exact item; the canonical flat shape itself
        /// has no MCP-server slot.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        provider_metadata: ProviderMetadata,
    },
    /// A human-in-the-loop **tool-approval response** — the V3
    /// `LanguageModelV3ToolApprovalResponsePart`. Supplied back by the
    /// client/tool layer (a `tool`-role input part) to grant or deny a
    /// provider-executed tool call that emitted a [`Self::ToolApprovalRequest`],
    /// keyed by the shared [`approval_id`](Self::ToolApprovalResponse::approval_id).
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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
            // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-prompt.ts>
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-source.ts>
///
/// V3 requires `id` on both variants, but providers rarely transmit a citation
/// id (OpenAI / Gemini / Anthropic web-search results carry none). Adapters
/// therefore **synthesize** a stable id from the source's URL and its index in
/// the response (see `synthesize_source_id` in each adapter) so the required
/// field is always populated without inventing identity the wire never carried.
///
/// The enclosing [`Content::Source`] part carries the uniform
/// `provider_metadata` slot (the V3 `providerMetadata` mechanism), but the
/// specific Anthropic citation sub-fields — `cited_text` / `encrypted_index`
/// and a document citation's page/char ranges — are **not** lifted into it:
/// they are never parsed or rendered. That is why the text-to-source linkage
/// (`cited_text` and char/page offsets) cannot cross faithfully today; the loss
/// is inherent to the current parity target (V3 `Source` carries only
/// id/url/title/mediaType/filename), not to any one adapter.
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/anthropic/src/convert-to-anthropic-messages-prompt.ts>
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
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-function-tool.ts>
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-provider-tool.ts>
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
///   faithful-passthrough rule [`ToolChoice`] applies to provider-specific
///   shapes it cannot map, which stay in `extra`). See
///   [`provider_defined_native`](crate::language_model::protocol) for the shared
///   reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Tool {
    /// A client-side function tool: `{name, description?, parameters, strict?}`.
    /// `parameters` is V3's `inputSchema` (a JSON Schema). `strict` is V3's
    /// top-level strict-mode flag — captured from the wire where present so it
    /// is not lost across the canonical boundary.
    ///
    /// The V3 `LanguageModelV3FunctionTool.inputExamples`
    /// (`Array<{ input: JSONObject }>`) field has **no slot here, by design.**
    /// None of the four provider *request* wires (Chat Completions / Messages /
    /// Responses / Generate Content) carries per-tool input examples in its tool
    /// definition, so no `parse_request` could construct it and no
    /// `render_request` could emit it — an `input_examples` field would be both
    /// unconstructed and unconsumed dead code. (Same documented-N/A reasoning as
    /// the [`Content::ToolCall`] `dynamic` flag.)
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

/// Constraint on the shape of the model's response.
///
/// Mirrors the Vercel AI SDK V3 `responseFormat` call option. V3 has **no**
/// standalone response-format type file — the shape is inlined on
/// `LanguageModelV3CallOptions` as
/// `responseFormat?: { type: 'text' } | { type: 'json', schema?, name?, description? }`.
/// [`Self::JsonSchema`] is the `{ type: 'json' }` arm (carrying the optional
/// `schema` / `name` / `description`); the `{ type: 'text' }` arm is the absence
/// of a constraint here (a `None` [`Prompt::response_format`]), so it needs no
/// variant. Future variants (`json_object`, `regex`) can be added without
/// breaking existing call sites.
///
/// Each inbound adapter promotes the provider-native field into this typed
/// slot at `parse_request` time (e.g. Chat Completions' `response_format`,
/// Messages' `output_config.format`, Generate Content's `generationConfig.responseSchema`).
/// Each outbound adapter renders it back into the upstream's native shape on
/// `render_request`. Cross-protocol routing therefore works automatically:
/// a Chat Completions client asking for `json_schema` against a Messages upstream
/// emits `output_config.format`.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-call-options.ts>
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Constrain output to a JSON Schema.
    JsonSchema {
        /// Schema name. Required by Chat Completions / Responses; ignored by
        /// Messages and Generate Content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Optional schema description — the V3 `responseFormat.description`.
        /// Used by some providers as extra LLM guidance for the structured
        /// output. Carried by the OpenAI family only: Chat Completions'
        /// `response_format.json_schema.description` and Responses'
        /// `text.format.description`. Anthropic `output_config.format` and
        /// Gemini `responseSchema` carry no schema-level description, so it is
        /// `None` after a round-trip through those wires (the same
        /// OpenAI-family-only treatment as [`name`](Self::JsonSchema::name)).
        /// <https://platform.openai.com/docs/guides/structured-outputs>
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Strict-mode flag. Chat Completions / Responses only; Messages and Generate Content are always strict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
        /// The JSON Schema.
        schema: serde_json::Value,
    },
}

/// Constraint on whether — and which — tool the model may call.
///
/// Like [`ResponseFormat`], each inbound adapter promotes the provider-native
/// `tool_choice` (Generate Content: `tool_config.function_calling_config`) into
/// this typed slot at `parse_request` time, and each outbound adapter renders it
/// back into the upstream's native shape on `render_request`. Cross-protocol
/// routing therefore translates automatically: an Anthropic Messages client
/// sending `tool_choice: {"type":"auto"}` against an OpenAI Chat Completions
/// upstream emits the bare string `"auto"` — not the object form, which OpenAI
/// rejects (the v0 #547 bug). Provider-specific shapes a given adapter can't map
/// (e.g. Responses hosted-tool selectors) are left in `extra` and pass through
/// opaquely, exactly as before.
///
/// Parallel-tool-use control is a distinct concern, not part of this slot. The
/// Messages adapter translates Anthropic's nested `disable_parallel_tool_use`
/// to/from the protocol-neutral top-level `parallel_tool_calls` (the shape Chat
/// Completions / Responses use), which rides `extra`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides freely whether to call a tool. Chat Completions /
    /// Responses `"auto"`, Messages `{"type":"auto"}`, Generate Content `AUTO`.
    Auto,
    /// The model must call at least one tool. Chat Completions / Responses
    /// `"required"`, Messages `{"type":"any"}`, Generate Content `ANY`.
    Required,
    /// The model must not call any tool. Chat Completions / Responses `"none"`,
    /// Messages `{"type":"none"}`, Generate Content `NONE`.
    None,
    /// The model must call exactly this tool. Chat Completions
    /// `{"type":"function","function":{"name":…}}`, Responses
    /// `{"type":"function","name":…}`, Messages `{"type":"tool","name":…}`,
    /// Generate Content `ANY` + `allowedFunctionNames`.
    Tool {
        /// The tool the model is forced to call.
        name: String,
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
    /// Top-k sampling cutoff. Carried by Anthropic (`top_k`) and Gemini
    /// (`generationConfig.topK`); Chat Completions and Responses have no such
    /// wire field and never render it. Like every typed sampling slot below,
    /// each inbound adapter promotes its native field here and removes it from
    /// `extra`; each outbound adapter renders it into the target's native shape,
    /// so it translates across protocols instead of no-op'ing through `extra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Deterministic-sampling seed. Carried by Chat Completions (`seed`) and
    /// Gemini (`generationConfig.seed`); Anthropic and Responses have no wire
    /// field for it. `i64` matches the JSON integer range these wires accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Stop sequences — generation halts when any is produced. Empty means none.
    /// Renders as Chat Completions `stop`, Anthropic `stop_sequences`, and Gemini
    /// `generationConfig.stopSequences`; Responses has no wire field for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Presence penalty. Carried by Chat Completions (`presence_penalty`) and
    /// Gemini (`generationConfig.presencePenalty`); Anthropic and Responses have
    /// no wire field for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Frequency penalty. Carried by Chat Completions (`frequency_penalty`) and
    /// Gemini (`generationConfig.frequencyPenalty`); Anthropic and Responses have
    /// no wire field for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
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
    /// Constraint on whether / which tool the model may call. Inbound adapters
    /// promote the provider-native `tool_choice` into this slot; outbound
    /// adapters render it back natively, so it translates across protocols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
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
///
/// Token usage, carrying the same information as the Vercel AI SDK V3
/// `LanguageModelV3Usage` in a **flat** shape suited to a router.
///
/// V3 itself models usage as a **nested** record —
/// `inputTokens: { total, noCache, cacheRead, cacheWrite }`,
/// `outputTokens: { total, text, reasoning }`, plus an optional `raw` blob.
/// bitrouter is a faithful-passthrough router: it carries the upstream's wire
/// counts and never has to hand a client back a `LanguageModelV3Usage` *object*,
/// so a flat field set is the more convenient internal form. Every V3 number is
/// either stored directly or derivable from the flat fields:
///
/// - V3 `inputTokens.total` → [`prompt_tokens`](Self::prompt_tokens). The cache
///   buckets are *included* in this total (folded in at parse time — see the
///   Messages `parse_usage`), matching V3's "total input" semantics.
/// - V3 `inputTokens.cacheRead` → [`cache_read_tokens`](Self::cache_read_tokens),
///   a subset of `prompt_tokens`.
/// - V3 `inputTokens.cacheWrite` → [`cache_write_tokens`](Self::cache_write_tokens),
///   a subset of `prompt_tokens`. (The earlier flat `Usage` had no cache-write
///   slot; this field restores the V3 `cacheWrite` bucket so a billing layer can
///   meter cache creation distinctly from cache reads.)
/// - V3 `inputTokens.noCache` is **derivable**, not stored:
///   `prompt_tokens - cache_read_tokens - cache_write_tokens`. Storing it too
///   would be a redundant field that could disagree with its own components.
/// - V3 `outputTokens.total` → [`completion_tokens`](Self::completion_tokens).
/// - V3 `outputTokens.reasoning` → [`reasoning_tokens`](Self::reasoning_tokens),
///   a subset of `completion_tokens`.
/// - V3 `outputTokens.text` is **derivable**, not stored:
///   `completion_tokens - reasoning_tokens`.
///
/// V3 carries no top-level `totalTokens`; the grand total is purely
/// `prompt_tokens + completion_tokens` ([`total`](Self::total)) here, computed on
/// demand rather than stored so it can never drift from its components. The V3
/// `raw` provider blob is not retained on this struct — a raw provider field that
/// lacks a canonical slot rides instead in
/// [`GenerateResult::provider_metadata`](crate::language_model::GenerateResult).
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-usage.ts>
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
    /// Provider-executed web-search calls this turn. Maps to Anthropic
    /// Messages `usage.server_tool_use.web_search_requests`
    /// (<https://docs.anthropic.com/en/api/messages>). Default 0 when the
    /// upstream reports none. Observability only — not a billing input.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub web_search_count: u64,
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

/// Which side executed a server tool. `Router` = a bitrouter router tool the
/// server-tool loop ran itself; `Provider` = a provider-executed tool the
/// upstream ran (e.g. Anthropic web search).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerToolKind {
    /// A tool the router executed on behalf of the model.
    Router,
    /// A tool the upstream provider executed (e.g. a web search).
    Provider,
}

/// Terminal status of a single server-tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerToolStatus {
    /// The tool completed successfully.
    Ok,
    /// The tool returned an error.
    Error,
    /// The tool call was denied (e.g. by a policy hook).
    Denied,
    /// The tool call timed out.
    Timeout,
}

/// One server-tool call observed during a request, for observability only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolCall {
    /// Tool name (`web_search`, `subagent`, `advisor`, …).
    pub name: String,
    /// Which side executed the tool.
    pub kind: ServerToolKind,
    /// Provider/loop call id where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Terminal status of this call.
    pub status: ServerToolStatus,
    /// Results produced by this call (e.g. web searches); 0 when N/A.
    #[serde(default)]
    pub result_count: u32,
}

/// Why generation stopped.
///
/// The Vercel AI SDK V3 `LanguageModelV3FinishReason` is a record carrying
/// **both** a `unified` enum (`stop` / `length` / `content-filter` /
/// `tool-calls` / `error` / `other`) and the provider's `raw` finish-reason
/// string. bitrouter splits the two: this enum *is* the V3 `unified` reason, and
/// the `raw` string rides **out-of-band** in
/// [`GenerateResult::provider_metadata`](GenerateResult)`["<provider>"]["rawFinishReason"]`.
/// Carrying `raw` separately — and only when the unified mapping is lossy (when
/// several native reasons collapse onto one variant, e.g. Anthropic
/// `stop_sequence` and `end_turn` both → [`Self::Stop`]) — lets a same-protocol
/// round-trip reproduce the exact native string while a cross-protocol route
/// still has a portable unified reason; reasons that already map losslessly
/// (e.g. Gemini `STOP`) store nothing. See
/// [`GenerateResult::provider_metadata`](GenerateResult) for the full
/// stash/restore contract.
///
/// `Other` and `Error` are escape valves: a finish reason the canonical set
/// doesn't model (kept verbatim for observability — the V3 `other` case), or a
/// mid-stream upstream failure surfaced through the canonical IR rather than
/// abruptly aborting the stream (the V3 `error` case).
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-finish-reason.ts>
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
///
/// **Relationship to the Vercel AI SDK V3 `LanguageModelV3GenerateResult`.** This
/// type carries `content`, `usage`, `finishReason`, `providerMetadata`, and the
/// `response.id` (as [`response_id`](Self::response_id)). Three V3 result members
/// are intentionally **not** modeled as fields here, each for a concrete
/// no-dead-code reason:
///
/// - **`warnings`** (V3 `Array<SharedV3Warning>` — settings a provider could not
///   honor). bitrouter forwards provider requests **faithfully** and never gates
///   or silently substitutes a setting, so the "unsupported setting" class barely
///   arises; the residual cross-protocol degrades (e.g. a function `strict` flag
///   dropped when routing to Anthropic/Gemini) happen inside
///   [`render_request`](crate::language_model::protocol::OutboundAdapter::render_request),
///   which returns only a request body and has **no channel** to the separate
///   [`parse_response`](crate::language_model::protocol::OutboundAdapter::parse_response)
///   that builds this result. Moreover none of the four client-facing response
///   wires (Chat Completions / Messages / Responses / Generate Content) has a
///   `warnings` slot in its **response** body, so a warning could never be
///   emitted back to a client either. A `warnings` field would therefore be both
///   unconstructed and unconsumed — pure dead weight — so it is omitted rather
///   than added empty.
/// - **`response.modelId`** and **`response.timestamp`**. The model id is already
///   surfaced from the routed [`ExecutionResult::model_id`] (which observability
///   stamps onto `gen_ai.response.model`), and Gemini's wire `modelVersion` rides
///   [`Self::provider_metadata`]`["google"]`; no consumer needs the raw echoed
///   `model`/`created` of the response body, so storing them here would be a dead
///   field. Raw `response.headers` and `request.body` are out of scope (telemetry
///   passthrough, not part of the canonical result).
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
    /// (`provider_metadata["openai"]["systemFingerprint"]`), Gemini
    /// `modelVersion` (`provider_metadata["google"]["modelVersion"]`), and the
    /// **raw provider finish reason** under
    /// `provider_metadata["<provider>"]["rawFinishReason"]` for finish reasons
    /// that the unified [`FinishReason`] enum cannot reproduce on its own. The
    /// canonical enum maps several native reasons onto one variant (Anthropic
    /// `stop_sequence` and `end_turn` both → [`FinishReason::Stop`]; Gemini
    /// `RECITATION` / `BLOCKLIST` / `PROHIBITED_CONTENT` and `SAFETY` all →
    /// [`FinishReason::ContentFilter`]; Chat Completions `function_call` →
    /// [`FinishReason::ToolCalls`]), so rendering from the enum alone would lose
    /// the exact native string on a same-protocol round-trip. The lossy
    /// mappings stash the raw string here at parse time, and each adapter's
    /// `render_response` reads it back (preferring it over the enum mapping) so
    /// the native finish reason survives byte-for-byte — and so observability
    /// can surface the precise upstream reason. Reasons that already map
    /// losslessly (e.g. `STOP`, `MAX_TOKENS`) store nothing.
    /// <https://platform.openai.com/docs/api-reference/chat/object>
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_metadata: ProviderMetadata,
}

/// One part of a streaming response, in canonical internal form. `StreamHook`
/// operates on a `Stream<Item = StreamPart>` before outbound protocol
/// conversion.
///
/// **Relationship to the Vercel AI SDK V3 `LanguageModelV3StreamPart`.**
/// bitrouter is a re-encoder: it decodes an upstream's SSE into these canonical
/// parts and re-encodes them into the client's own SSE lifecycle, so a variant
/// is modeled only when it changes the *re-encoded* bytes. The block-lifecycle
/// members are carried because two of the client wires frame content into
/// explicit blocks: text-start/end ([`Self::TextStart`] / [`Self::TextEnd`]) and
/// reasoning-start/end ([`Self::ReasoningStart`] / [`Self::ReasoningEnd`]) fix a
/// concrete merged-block bug on the Anthropic Messages and OpenAI Responses
/// encoders (see [`Self::TextStart`]). The following V3 members are
/// deliberately **not** modeled, each for a no-dead-code reason:
///
/// - **`tool-input-start` / `tool-input-end`.** Tool-argument streaming is
///   already framed losslessly without dedicated markers: a
///   [`Self::ToolCallDelta`] whose `name` is `Some` *is* the start signal — both
///   block-framed encoders force-close the open block and open a new tool block
///   on it, so consecutive tool calls land in distinct blocks (Anthropic
///   `tool_use`, Responses `function_call`), and the block closes on the next
///   block/terminal. A separate `tool-input-end` would change no re-encoded
///   byte, so it would be unconstructed-or-unconsumed churn.
/// - **`stream-start { warnings }`.** Warnings describe settings a provider
///   could not honor; bitrouter forwards requests faithfully and never gates a
///   setting, and no client *response* wire has a streaming `warnings` slot, so
///   such a part could be neither meaningfully produced nor emitted — the same
///   reasoning that omits `warnings` from [`GenerateResult`].
/// - **`raw` (`includeRawChunks`).** bitrouter re-encodes canonical parts into
///   the client's wire; it never tunnels an upstream's raw chunk through, so a
///   `raw` part would have no encoder that could emit it.
/// - **`response-metadata` (mid-stream id/timestamp/modelId).** The response id
///   is already surfaced once as [`Self::ResponseStarted`] (and on
///   [`Self::ResponseCompleted`] for Responses); no client encoder consumes a
///   mid-stream model-id/timestamp refresh, so a dedicated part would be dead.
/// - **`error`.** A terminal upstream error is modeled as
///   [`FinishReason::Error`] and surfaced through each encoder's
///   `encode_error` (protocol-shaped terminal frame: Anthropic `error`, Chat
///   error chunk, Responses `response.failed`). A separate `error` `StreamPart`
///   would duplicate that path with no extra fidelity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamPart {
    /// Opens a text block — the V3 `text-start` part. Carries the upstream
    /// block id so a client encoder that frames text into explicit blocks
    /// (Anthropic `content_block_start`, Responses `output_item.added` +
    /// `content_part.added`) opens a *fresh* block here rather than inferring
    /// the boundary from delta transitions.
    ///
    /// **Why this exists (the merged-block fix).** Without an explicit start/end
    /// pair the canonical stream is a flat run of [`Self::TextDelta`]s, so two
    /// *distinct* upstream text blocks (e.g. Anthropic `content_block` index 0
    /// then index 2, or two Responses `message` items) decode to
    /// `TextDelta`,`TextDelta` with no separator and re-encode into a **single**
    /// merged block — a real fidelity loss on the two block-framed wires.
    /// The decoder now emits this marker on each `content_block_start` (text) /
    /// message-item open, and the encoder closes-then-reopens on it, so block
    /// boundaries survive a same-protocol round trip. The two coarse wires
    /// (Chat Completions / Generate Content) carry no block frame, so their
    /// decoders never emit it and their encoders treat it as a no-op.
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-stream-part.ts>
    TextStart {
        /// Upstream block id (Anthropic block index rendered as a string, or a
        /// Responses item id). Stable within one stream; used by a framing
        /// encoder to correlate the matching [`Self::TextEnd`].
        id: String,
    },
    /// An incremental chunk of assistant text.
    TextDelta {
        /// The text fragment.
        text: String,
    },
    /// Closes the text block opened by the matching [`Self::TextStart`] — the V3
    /// `text-end` part. A framing encoder emits its block-close frame here
    /// (Anthropic `content_block_stop`, Responses `output_text.done` +
    /// `content_part.done` + `output_item.done`); the coarse wires no-op it.
    TextEnd {
        /// Block id, matching the opening [`Self::TextStart`].
        id: String,
    },
    /// Opens a reasoning / thinking block — the V3 `reasoning-start` part. The
    /// reasoning counterpart of [`Self::TextStart`]; see its docs for the
    /// merged-block rationale. Decoded from Anthropic `thinking` /
    /// `redacted_thinking` `content_block_start` and Responses `reasoning`
    /// item opens.
    ReasoningStart {
        /// Upstream reasoning-block id.
        id: String,
    },
    /// An incremental chunk of reasoning / thinking text.
    ReasoningDelta {
        /// The reasoning fragment.
        text: String,
    },
    /// Closes the reasoning block opened by the matching [`Self::ReasoningStart`]
    /// — the V3 `reasoning-end` part.
    ReasoningEnd {
        /// Reasoning-block id, matching the opening [`Self::ReasoningStart`].
        id: String,
        /// The reasoning block's opaque continuity signature, when the upstream
        /// emitted one — Anthropic's thinking-block `signature`, carried on the
        /// terminal `signature_delta`. Preserved verbatim so a streamed thinking
        /// block re-encodes signed and a follow-up turn that replays it validates
        /// (without it Anthropic rejects the unsigned block). `None` on wires or
        /// blocks that carry no signature.
        /// <https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking>
        signature: Option<String>,
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
    /// A complete **provider/router-executed** tool call, emitted whole (not as
    /// [`Self::ToolCallDelta`] fragments). The server-side tool loop emits this
    /// for a tool BitRouter executed itself; a framing encoder renders it as the
    /// protocol's native server-tool / MCP call block (Anthropic
    /// `server_tool_use` / `mcp_tool_use`, Responses `mcp_call`). The coarse
    /// wires (Chat Completions / Generate Content) degrade it — see each
    /// encoder. `dynamic` marks an MCP (Model Context Protocol) tool, mirroring
    /// [`Content::ToolCall::dynamic`].
    ServerToolCall {
        /// Provider-assigned call id, paired with the matching
        /// [`Self::ServerToolResult`].
        id: String,
        /// Tool name.
        name: String,
        /// JSON-encoded arguments, whole.
        arguments: String,
        /// The MCP server that owns the tool, when known. Lets a framing
        /// encoder reproduce the Anthropic `mcp_tool_use` block's `server_name`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        /// Whether this is a dynamic (MCP) tool call.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dynamic: bool,
    },
    /// The result of a [`Self::ServerToolCall`], emitted whole. Rendered as the
    /// protocol's native server-tool / MCP result block (Anthropic
    /// `mcp_tool_result` / `web_search_tool_result`, Responses `mcp_call`
    /// output); degraded on the coarse wires. Mirrors [`Content::ToolResult`].
    ServerToolResult {
        /// The [`Self::ServerToolCall`] id this result answers.
        call_id: String,
        /// The tool's name, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// The typed result body.
        output: ToolResultOutput,
        /// Whether this answers a dynamic (MCP) tool call.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dynamic: bool,
    },
    /// A complete generated file (e.g. an image), emitted whole — matching the
    /// Vercel AI SDK `LanguageModelV3` stream `file` part, where files arrive as
    /// one part rather than chunked deltas.
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-file.ts>
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
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/provider/src/language-model/v3/language-model-v3-source.ts>
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
    /// Server-tool calls observed during this execution (router-executed and
    /// provider-executed). Empty for a plain single-turn upstream call.
    /// Observability only.
    pub server_tool_calls: Vec<ServerToolCall>,
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
    /// The wire protocol the request arrived on, when known — set by the HTTP
    /// server from the endpoint that was hit. Lets the router prefer a
    /// same-protocol (native) upstream so a faithful round-trip replaces a
    /// lossy cross-protocol translation. `None` for callers that build a
    /// request directly (no native preference is then applied).
    pub inbound_protocol: Option<ApiProtocol>,
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
            inbound_protocol: None,
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
            tool_choice: None,
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
            description: None,
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
            description: None,
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

    #[test]
    fn server_tool_call_roundtrips() {
        let c = ServerToolCall {
            name: "web_search".into(),
            kind: ServerToolKind::Provider,
            call_id: Some("srvtoolu_1".into()),
            status: ServerToolStatus::Ok,
            result_count: 2,
        };
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["kind"], "provider");
        assert_eq!(j["status"], "ok");
        let back: ServerToolCall = serde_json::from_value(j).unwrap();
        assert_eq!(back, c);
    }
}
