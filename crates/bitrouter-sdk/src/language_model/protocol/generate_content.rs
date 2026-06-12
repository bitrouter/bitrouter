//! Generate Content (`generateContent`) adapter.
//!
//! Official reference: <https://ai.google.dev/api/generate-content>
//! Streaming: <https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent>
//!
//! Google uses `contents[]` with roles `user` / `model`, a separate
//! `systemInstruction`, and `parts[]` of `{text}` / `{functionCall}` /
//! `{functionResponse}`. Reasoning is a `part` flagged `thought: true`
//! (v0 #454-1: such parts must not be dropped).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, PROVIDER_ID_GOOGLE, SseEvent, StreamDecoder, StreamEncoder,
    Transport, describe_deser_error, provider_defined_native, rendered_finish_reason,
    stash_raw_finish_reason,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, DataContent, FinishReason, GenerateResult, GenerationParams, Message,
    Modality, Prompt, ProviderMetadata, ResponseFormat, Role, RoutingTarget, Source, StreamPart,
    Tool, ToolChoice, ToolResultOutput, Usage, provider_namespace, set_provider_metadata,
};

/// The metadata key, within the `google` namespace, carrying a reasoning part's
/// `thoughtSignature` — the continuity token Gemini emits on thinking parts so a
/// follow-up turn can replay the reasoning. Matches the Vercel AI SDK's
/// `providerMetadata.google.thoughtSignature`.
/// <https://ai.google.dev/gemini-api/docs/thinking>
const GOOGLE_THOUGHT_SIGNATURE: &str = "thoughtSignature";
/// The metadata key, within the `google` namespace, carrying the response's
/// `modelVersion` (the exact model build that served the response) at result
/// level — it has no dedicated canonical field.
/// <https://ai.google.dev/api/generate-content#GenerateContentResponse>
const GOOGLE_MODEL_VERSION: &str = "modelVersion";

/// The Generate Content protocol adapter.
pub struct GenerateContentAdapter;

/// HTTP transport for Generate Content:
/// `POST {api_base}/models/{model}:generateContent` (or
/// `:streamGenerateContent?alt=sse` for streaming) with the `x-goog-api-key`
/// header — documented at
/// <https://ai.google.dev/gemini-api/docs/api-key>.
pub struct GenerateContentTransport;

/// Sentinel key under which top-level Google extras (`toolConfig`,
/// `safetySettings`, `cachedContent`, …) ride through `GenerationParams::extra`.
/// Only `GenerateContentAdapter::render_request` reads it — every other adapter ignores
/// the namespaced key and the JSON wire shape never contains it.
const GOOGLE_TOP_LEVEL_EXTRA_KEY: &str = "__google_top_level__";

// ===== wire request types =====

/// Generate Content `generateContent` request body
/// (<https://ai.google.dev/api/generate-content>).
///
/// `pub` so downstream crates (notably `bitrouter-cloud`) can derive an
/// OpenAPI schema from the canonical wire shape without redeclaring it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    /// Carried as a field so the inbound HTTP route's `{model}` path param can
    /// override it; defaults empty.
    #[serde(default)]
    model: String,
    contents: Vec<GenerateContentContent>,
    #[serde(default)]
    system_instruction: Option<GenerateContentContent>,
    #[serde(default)]
    tools: Vec<GenerateContentTool>,
    #[serde(default)]
    generation_config: Option<GenerateContentGenerationConfig>,
    /// Injected by `server::generate_content` from the path verb
    /// (`:streamGenerateContent` → true, `:generateContent` → false).
    #[serde(default)]
    stream: bool,
    /// Top-level extras: `toolConfig`, `safetySettings`, `cachedContent`, … —
    /// preserve them across the inbound→outbound round-trip. Per
    /// <https://ai.google.dev/api/generate-content>. Skipped from the
    /// published schema so the documented contract is the set of typed
    /// fields; pass-through behavior is preserved at runtime.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// One element of [`GenerateContentRequest`]'s `contents` array — a turn
/// carrying optional role + `parts[]`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateContentContent {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    parts: Vec<serde_json::Value>,
}

/// One element of [`GenerateContentRequest`]'s `tools` array.
///
/// A single Google tool object may carry client function declarations
/// (`functionDeclarations`) **and/or** one or more provider-defined ("built-in")
/// tool keys on the same object — `googleSearch`, `codeExecution`,
/// `googleSearchRetrieval`, `urlContext`. Function declarations parse to
/// [`Tool::Function`]; every other key is a provider-defined tool namespaced
/// `google.<key>`, its value preserved verbatim as `args`. The non-function keys
/// ride in `extra`.
/// <https://ai.google.dev/gemini-api/docs/function-calling>
/// <https://ai.google.dev/gemini-api/docs/google-search>
/// <https://ai.google.dev/gemini-api/docs/code-execution>
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentTool {
    #[serde(default)]
    function_declarations: Vec<GenerateContentFunctionDecl>,
    /// Provider-defined (built-in) tool keys on this object — `googleSearch`,
    /// `codeExecution`, `googleSearchRetrieval`, `urlContext`, … — each preserved
    /// verbatim. Skipped from the published schema; the documented contract is
    /// the typed `functionDeclarations` shape.
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// One function declaration inside a [`GenerateContentTool`]: name + description +
/// JSON-Schema parameters.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateContentFunctionDecl {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
}

/// `generationConfig` knobs on a [`GenerateContentRequest`].
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentGenerationConfig {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    /// Response MIME type. `application/json` paired with `response_schema` is
    /// the structured-output contract; other values (e.g. `text/x.enum`) pass
    /// through opaquely.
    /// <https://ai.google.dev/gemini-api/docs/structured-output>
    #[serde(default)]
    response_mime_type: Option<String>,
    /// JSON Schema constraining the response (with
    /// `response_mime_type: "application/json"`).
    #[serde(default)]
    response_schema: Option<serde_json::Value>,
    /// Top-k sampling. Promoted to the canonical `top_k` slot so it translates
    /// across protocols (e.g. to an Anthropic upstream's `top_k`).
    /// <https://ai.google.dev/api/generate-content#GenerationConfig>
    #[serde(default)]
    top_k: Option<u32>,
    /// Deterministic-sampling seed. Promoted to the canonical `seed` slot.
    /// <https://ai.google.dev/api/generate-content#GenerationConfig>
    #[serde(default)]
    seed: Option<i64>,
    /// Stop sequences. Promoted to the canonical `stop` slot, so it can render
    /// as a Chat Completions `stop` or an Anthropic `stop_sequences`.
    /// <https://ai.google.dev/api/generate-content#GenerationConfig>
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    /// Presence penalty. Promoted to the canonical `presence_penalty` slot.
    /// <https://ai.google.dev/api/generate-content#GenerationConfig>
    #[serde(default)]
    presence_penalty: Option<f64>,
    /// Frequency penalty. Promoted to the canonical `frequency_penalty` slot.
    /// <https://ai.google.dev/api/generate-content#GenerationConfig>
    #[serde(default)]
    frequency_penalty: Option<f64>,
    /// `responseLogprobs`, `candidateCount`, `thinkingConfig`, … — every
    /// generation-config knob without a typed slot rides via `extra` and is
    /// splatted back into `generationConfig` on render. v0 passed these through.
    /// Skipped from the published schema for the same reason as the top-level
    /// `extra` on [`GenerateContentRequest`].
    #[serde(flatten)]
    #[schemars(skip)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Map a Google role to canonical. Total mapping — unknown roles error (#454-4).
fn parse_role(role: Option<&str>) -> Result<Role> {
    match role {
        Some("user") | None => Ok(Role::User),
        Some("model") => Ok(Role::Assistant),
        Some("function") | Some("tool") => Ok(Role::Tool),
        Some(other) => Err(BitrouterError::bad_request(format!(
            "unknown google content role '{other}' (expected user/model)"
        ))),
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Assistant => "model",
        // Generate Content has only user/model; tool results ride in a user turn.
        Role::User | Role::System | Role::Tool => "user",
    }
}

/// Map a Google `responseModalities` token (uppercase) to a canonical [`Modality`].
fn modality_from_gemini(token: &str) -> Option<Modality> {
    match token {
        "TEXT" => Some(Modality::Text),
        "IMAGE" => Some(Modality::Image),
        "AUDIO" => Some(Modality::Audio),
        _ => None,
    }
}

/// The Google `responseModalities` token (uppercase) for a canonical [`Modality`].
fn modality_to_gemini(modality: &Modality) -> &'static str {
    match modality {
        Modality::Text => "TEXT",
        Modality::Image => "IMAGE",
        Modality::Audio => "AUDIO",
    }
}

/// Take `responseModalities` out of a generation-config extras map, mapping the
/// uppercase Google tokens to canonical modalities.
fn take_gemini_modalities(
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) -> Vec<Modality> {
    extra
        .remove("responseModalities")
        .and_then(|v| {
            v.as_array().map(|tokens| {
                tokens
                    .iter()
                    .filter_map(|t| t.as_str())
                    .filter_map(modality_from_gemini)
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Expand one Google `tools[]` object into canonical [`Tool`]s: each
/// `functionDeclarations` entry → a [`Tool::Function`]; each other (built-in)
/// key → a [`Tool::ProviderDefined`] namespaced `google.<key>`, value preserved
/// verbatim as `args`.
fn parse_generate_content_tool(t: GenerateContentTool) -> Vec<Tool> {
    let mut out: Vec<Tool> = t
        .function_declarations
        .into_iter()
        .map(|f| Tool::Function {
            name: f.name,
            description: f.description,
            parameters: f.parameters,
            // Google function declarations carry no `strict` slot.
            strict: None,
            // Gemini has no per-tool `cache_control`; no metadata to lift.
            provider_metadata: ProviderMetadata::new(),
        })
        .collect();
    for (key, value) in t.extra {
        out.push(Tool::ProviderDefined {
            id: format!("{PROVIDER_ID_GOOGLE}.{key}"),
            name: key,
            args: value,
            provider_metadata: ProviderMetadata::new(),
        });
    }
    out
}

/// Render the canonical tool list into Google's `tools` value.
///
/// Google packs all client function tools into a single object's
/// `functionDeclarations` array, while each provider-defined built-in tool is a
/// distinct single-key object (`{googleSearch:{}}`, `{codeExecution:{}}`, …).
/// Google's own function declarations have **no** `strict` slot, so
/// [`Tool::Function::strict`] is intentionally dropped here (documented; mirrors
/// the structured-output `strict` drop). A [`Tool::ProviderDefined`] renders to
/// its source-native shape via [`provider_defined_native`]: a `google.*` id is a
/// lossless same-protocol round-trip; a foreign-provider id is preserved verbatim
/// (faithful passthrough) as its own tool-array element so the upstream decides.
/// <https://ai.google.dev/gemini-api/docs/function-calling>
/// <https://ai.google.dev/gemini-api/docs/google-search>
fn render_generate_content_tools(tools: &[Tool]) -> serde_json::Value {
    let mut function_declarations = Vec::new();
    let mut entries = Vec::new();
    for tool in tools {
        match tool {
            Tool::Function {
                name,
                description,
                parameters,
                strict: _,
                ..
            } => function_declarations.push(serde_json::json!({
                "name": name,
                "description": description,
                "parameters": parameters,
            })),
            Tool::ProviderDefined { id, name, args, .. } => {
                entries.push(provider_defined_native(id, name, args));
            }
        }
    }
    // The function-declarations object goes first when present, preserving the
    // prior single-object render for the common (function-only) case.
    let mut out = Vec::with_capacity(entries.len() + 1);
    if !function_declarations.is_empty() {
        out.push(serde_json::json!({ "functionDeclarations": function_declarations }));
    }
    out.extend(entries);
    serde_json::Value::Array(out)
}

/// Lift a Gemini part's `thoughtSignature` into a [`ProviderMetadata`] under the
/// `google` namespace. Gemini stamps this opaque token on thinking parts (and on
/// the `functionCall` parts that continue a reasoning chain); it must round-trip
/// or a follow-up turn replaying the reasoning is rejected. Returns an empty map
/// when the part has none.
/// <https://ai.google.dev/gemini-api/docs/thinking>
fn parse_thought_signature(part: &serde_json::Value) -> ProviderMetadata {
    let mut meta = ProviderMetadata::new();
    if let Some(sig) = part.get("thoughtSignature").filter(|v| !v.is_null()) {
        set_provider_metadata(
            &mut meta,
            PROVIDER_ID_GOOGLE,
            GOOGLE_THOUGHT_SIGNATURE,
            sig.clone(),
        );
    }
    meta
}

/// Splat a Gemini `thoughtSignature` from `meta` onto an existing part JSON
/// object (a no-op when there is none) — the inverse of
/// [`parse_thought_signature`], used by the render path.
fn apply_thought_signature(target: &mut serde_json::Value, meta: &ProviderMetadata) {
    if let Some(sig) =
        provider_namespace(meta, PROVIDER_ID_GOOGLE).and_then(|o| o.get(GOOGLE_THOUGHT_SIGNATURE))
        && let Some(obj) = target.as_object_mut()
    {
        obj.insert("thoughtSignature".to_string(), sig.clone());
    }
}

/// Parse one Google `parts[]` array into ordered canonical content. Order is
/// preserved (#416); `thought: true` parts become `Reasoning` (#454-1).
fn parse_parts(parts: &[serde_json::Value]) -> Vec<Content> {
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        if let Some(fc) = part.get("functionCall") {
            out.push(Content::ToolCall {
                id: fc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                arguments: fc
                    .get("args")
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "{}".to_string()),
                // Gemini `functionCall` parts are client tool calls. Google's
                // own server-side tools (Search grounding, code execution)
                // surface as separate `groundingMetadata` / `executableCode`
                // fields, never as a `functionCall`, so there is no
                // provider-executed call to parse here.
                provider_executed: false,
                // Gemini has no provider-executed MCP (`dynamic`) call envelope.
                dynamic: false,
                // A `functionCall` part may carry a `thoughtSignature` (it
                // continues a reasoning chain); preserve it so a follow-up turn
                // can replay the chain.
                // <https://ai.google.dev/gemini-api/docs/thinking>
                provider_metadata: parse_thought_signature(part),
            });
        } else if let Some(fr) = part.get("functionResponse") {
            // Gemini `functionResponse {id?, name, response}`: `name` is the tool
            // name (required), the optional `id` correlates the originating call,
            // and `response` is a JSON object. Map `name` → tool_name, `id` →
            // call_id (falling back to `name` when the wire omits the id, since
            // the canonical call_id must not be empty), and the JSON `response`
            // → a structured Json output.
            // <https://ai.google.dev/api/caching#FunctionResponse>
            let name = fr
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let call_id = fr
                .get("id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| name.clone());
            let output = fr
                .get("response")
                .map(ToolResultOutput::from_untyped_value)
                .unwrap_or_else(|| ToolResultOutput::Json {
                    value: serde_json::json!({}),
                });
            out.push(Content::ToolResult {
                call_id,
                tool_name: (!name.is_empty()).then_some(name),
                output,
                // Gemini has no MCP tool-result wire.
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            });
        } else if let Some(inline) = part.get("inlineData") {
            // Inline base64 media. <https://ai.google.dev/gemini-api/docs/image-understanding>
            out.push(Content::File {
                media_type: inline
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
                data: DataContent::Base64 {
                    data: inline
                        .get("data")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                filename: None,
                provider_metadata: ProviderMetadata::new(),
            });
        } else if let Some(file) = part.get("fileData") {
            // A URI the model fetches.
            out.push(Content::File {
                media_type: file
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
                data: DataContent::Url {
                    url: file
                        .get("fileUri")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string(),
                },
                filename: None,
                provider_metadata: ProviderMetadata::new(),
            });
        } else if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            if is_thought {
                out.push(Content::Reasoning {
                    text: text.to_string(),
                    // Preserve the thinking part's `thoughtSignature` continuity
                    // token so reasoning round-trips into a follow-up turn.
                    // <https://ai.google.dev/gemini-api/docs/thinking>
                    provider_metadata: parse_thought_signature(part),
                });
            } else {
                out.push(Content::Text {
                    text: text.to_string(),
                    provider_metadata: ProviderMetadata::new(),
                });
            }
        }
    }
    out
}

/// Infer a document IANA media type from a Gemini `retrievedContext` file
/// path's extension (e.g. a `gs://…/report.pdf` uri). Mirrors the AI SDK's
/// extension table; an unrecognized extension falls back to
/// `application/octet-stream`.
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/google/src/google-generative-ai-language-model.ts>
fn grounding_doc_media_type(uri: &str) -> &'static str {
    if uri.ends_with(".pdf") {
        "application/pdf"
    } else if uri.ends_with(".txt") {
        "text/plain"
    } else if uri.ends_with(".docx") {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    } else if uri.ends_with(".doc") {
        "application/msword"
    } else if uri.ends_with(".md") || uri.ends_with(".markdown") {
        "text/markdown"
    } else {
        "application/octet-stream"
    }
}

/// The trailing path segment of a `gs://`/file-path uri, used as a document
/// `filename` (mirrors the AI SDK's `uri.split('/').pop()`).
fn grounding_filename(uri: &str) -> Option<String> {
    uri.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse one Gemini grounding chunk into a canonical [`Content::Source`].
/// Mirrors the AI SDK `extractSources`, handling every chunk kind:
/// - `web` (`{uri, title?}`) → [`Source::Url`] — the server Search tool's hits;
/// - `image` (`{sourceUri, title?}`) → [`Source::Url`] keyed by `sourceUri`
///   (Google requires attribution to the source page, not the image bytes);
/// - `retrievedContext` (`{uri?, title?, fileSearchStore?}`) — an `http(s)` uri
///   → [`Source::Url`]; any other uri (e.g. `gs://`) → [`Source::Document`] with
///   the media type/filename inferred from the path; a `fileSearchStore`-only
///   chunk → [`Source::Document`] keyed by the store's trailing segment;
/// - `maps` (`{uri?, title?}`) → [`Source::Url`] when it carries a `uri`.
///
/// The chunk carries no citation id, so one is synthesized from the
/// url/filename + index. A chunk with no usable field yields `None` (e.g. a
/// `maps`/`retrievedContext` chunk with neither uri nor store) — these are the
/// only documented drops, and they carry nothing representable.
/// <https://ai.google.dev/api/generate-content#GroundingChunk>
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/google/src/google-generative-ai-language-model.ts>
fn parse_grounding_chunk(chunk: &serde_json::Value, index: usize) -> Option<Content> {
    let title_of = |obj: &serde_json::Value| {
        obj.get("title")
            .and_then(|t| t.as_str())
            .map(str::to_string)
    };
    let url_source = |url: String, title: Option<String>| Content::Source {
        source: Source::Url {
            id: Source::synthesize_id(&url, index),
            url,
            title,
        },
        provider_metadata: ProviderMetadata::new(),
    };
    if let Some(web) = chunk.get("web") {
        let url = web.get("uri").and_then(|u| u.as_str())?.to_string();
        return Some(url_source(url, title_of(web)));
    }
    if let Some(image) = chunk.get("image") {
        // Attribution uses the source page URI, not the raw `imageUri`.
        let url = image.get("sourceUri").and_then(|u| u.as_str())?.to_string();
        return Some(url_source(url, title_of(image)));
    }
    if let Some(ctx) = chunk.get("retrievedContext") {
        let uri = ctx.get("uri").and_then(|u| u.as_str());
        if let Some(uri) = uri {
            // An http(s) RAG source is representable as a URL today; a
            // file-path source (`gs://`, …) becomes a document citation.
            if uri.starts_with("http://") || uri.starts_with("https://") {
                return Some(url_source(uri.to_string(), title_of(ctx)));
            }
            let filename = grounding_filename(uri);
            let title = title_of(ctx).unwrap_or_else(|| "Unknown Document".to_string());
            return Some(Content::Source {
                source: Source::Document {
                    id: Source::synthesize_id(uri, index),
                    media_type: grounding_doc_media_type(uri).to_string(),
                    title,
                    filename,
                },
                provider_metadata: ProviderMetadata::new(),
            });
        }
        // File Search format: a store id with no uri.
        if let Some(store) = ctx.get("fileSearchStore").and_then(|s| s.as_str()) {
            let title = title_of(ctx).unwrap_or_else(|| "Unknown Document".to_string());
            return Some(Content::Source {
                source: Source::Document {
                    id: Source::synthesize_id(store, index),
                    media_type: "application/octet-stream".to_string(),
                    title,
                    filename: grounding_filename(store),
                },
                provider_metadata: ProviderMetadata::new(),
            });
        }
        return None;
    }
    if let Some(maps) = chunk.get("maps") {
        let url = maps.get("uri").and_then(|u| u.as_str())?.to_string();
        return Some(url_source(url, title_of(maps)));
    }
    // An unrecognized, forward-compatible chunk kind carries no representable
    // citation field. The only grounding chunk kinds Gemini documents are
    // `web`, `image`, `retrievedContext`, and `maps` (all handled above); a new
    // kind is skipped rather than silently mismapped.
    // <https://ai.google.dev/api/generate-content#GroundingChunk>
    None
}

/// Parse a Gemini `candidate.groundingMetadata` object into canonical
/// [`Content::Source`] parts. Web search grounding surfaces as
/// `groundingMetadata.groundingChunks[]`; the model's own server-side Search
/// tool emits these instead of any `functionCall`. Each chunk is mapped by
/// [`parse_grounding_chunk`] (every documented chunk kind is handled). The chunk
/// carries no citation id, so one is synthesized from the uri/filename + index.
/// Mirrors the AI SDK `extractSources`.
/// <https://ai.google.dev/gemini-api/docs/grounding>
/// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/google/src/google-generative-ai-language-model.ts>
fn parse_grounding_sources(grounding: Option<&serde_json::Value>) -> Vec<Content> {
    let Some(chunks) = grounding
        .and_then(|g| g.get("groundingChunks"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    chunks
        .iter()
        .enumerate()
        .filter_map(|(i, chunk)| parse_grounding_chunk(chunk, i))
        .collect()
}

/// Render canonical [`Content::Source`] parts into a Gemini
/// `groundingMetadata.groundingChunks[]` array (web chunks `{web:{uri, title}}`)
/// — the location [`parse_grounding_sources`] reads. Only [`Source::Url`] maps;
/// a [`Source::Document`] citation has no `groundingChunks.web` form and is
/// dropped (documented cross-protocol loss). Returns an empty Vec when the
/// result carries no URL sources.
/// <https://ai.google.dev/api/generate-content#GroundingChunk>
fn render_grounding_chunks(result: &GenerateResult) -> Vec<serde_json::Value> {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Source {
                source: Source::Url { url, title, .. },
                ..
            } => {
                let mut web = serde_json::Map::new();
                web.insert("uri".into(), url.clone().into());
                if let Some(title) = title {
                    web.insert("title".into(), title.clone().into());
                }
                Some(serde_json::json!({ "web": serde_json::Value::Object(web) }))
            }
            Content::Source {
                source: Source::Document { .. },
                ..
            } => None,
            _ => None,
        })
        .collect()
}

fn finish_reason(s: &str) -> Option<FinishReason> {
    match s {
        "STOP" => Some(FinishReason::Stop),
        "MAX_TOKENS" => Some(FinishReason::Length),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
            Some(FinishReason::ContentFilter)
        }
        other => Some(FinishReason::Other(other.to_string())),
    }
}

fn finish_reason_str(r: &FinishReason) -> String {
    match r {
        FinishReason::Stop => "STOP".to_string(),
        FinishReason::Length => "MAX_TOKENS".to_string(),
        FinishReason::ToolCalls => "STOP".to_string(),
        FinishReason::ContentFilter => "SAFETY".to_string(),
        FinishReason::Other(s) => s.clone(),
        FinishReason::Error(_) => "OTHER".to_string(),
    }
}

/// Promote Google's `toolConfig.functionCallingConfig` into the canonical
/// [`ToolChoice`], removing the function-calling config (and `toolConfig` itself
/// when it becomes empty) from the top-level `extra` map. `ANY` with exactly one
/// `allowedFunctionNames` maps to a forced single tool; `ANY` with none maps to
/// `Required`. `allowedFunctionNames` is a restricting set with no canonical
/// equivalent beyond that single-tool case, so shapes that would lose it — `ANY`
/// with two or more names, or `AUTO`/`NONE` carrying names — are left untouched
/// to pass through verbatim rather than silently widened. Unmapped modes are
/// likewise left untouched.
/// <https://ai.google.dev/api/caching#FunctionCallingConfig>
fn parse_gc_tool_choice(
    extra: &mut std::collections::HashMap<String, serde_json::Value>,
) -> Option<ToolChoice> {
    let tool_config = extra.get_mut("toolConfig")?.as_object_mut()?;
    let fcc = tool_config.get("functionCallingConfig")?.as_object()?;
    let mode = fcc
        .get("mode")
        .and_then(|m| m.as_str())
        .map(|s| s.to_ascii_uppercase());
    let names: Vec<String> = fcc
        .get("allowedFunctionNames")
        .and_then(|n| n.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let parsed = match mode.as_deref() {
        Some("AUTO") if names.is_empty() => Some(ToolChoice::Auto),
        Some("NONE") if names.is_empty() => Some(ToolChoice::None),
        Some("ANY") if names.is_empty() => Some(ToolChoice::Required),
        Some("ANY") if names.len() == 1 => Some(ToolChoice::Tool {
            name: names[0].clone(),
        }),
        _ => None,
    };
    let drop_tool_config = if parsed.is_some() {
        tool_config.remove("functionCallingConfig");
        tool_config.is_empty()
    } else {
        false
    };
    if drop_tool_config {
        extra.remove("toolConfig");
    }
    parsed
}

/// Render the canonical [`ToolChoice`] into Google's `functionCallingConfig`
/// body (`{ mode, allowedFunctionNames? }`).
fn render_gc_function_calling_config(tc: &ToolChoice) -> serde_json::Value {
    match tc {
        ToolChoice::Auto => serde_json::json!({ "mode": "AUTO" }),
        ToolChoice::Required => serde_json::json!({ "mode": "ANY" }),
        ToolChoice::None => serde_json::json!({ "mode": "NONE" }),
        ToolChoice::Tool { name } => serde_json::json!({
            "mode": "ANY",
            "allowedFunctionNames": [name],
        }),
    }
}

impl InboundAdapter for GenerateContentAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::GenerateContent
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        let mut req: GenerateContentRequest = serde_json::from_value(body.clone())
            .map_err(|e| describe_deser_error("GenerateContentRequest", &e, &body))?;

        let system = req.system_instruction.as_ref().map(|si| {
            si.parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        });

        let mut messages = Vec::with_capacity(req.contents.len());
        for c in &req.contents {
            let role = parse_role(c.role.as_deref())?;
            let parsed = parse_parts(&c.parts);
            let (tool_results, rest): (Vec<_>, Vec<_>) = parsed
                .into_iter()
                .partition(|x| matches!(x, Content::ToolResult { .. }));
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

        // A Google tool object may carry `functionDeclarations` (client function
        // tools) and/or provider-defined built-in keys (`googleSearch`,
        // `codeExecution`, `googleSearchRetrieval`, `urlContext`, …) on the same
        // object. Expand each object into its function tools plus one
        // `Tool::ProviderDefined` per built-in key, namespaced `google.<key>`,
        // value preserved verbatim as `args`.
        // <https://ai.google.dev/gemini-api/docs/function-calling>
        let tools = req
            .tools
            .into_iter()
            .flat_map(parse_generate_content_tool)
            .collect();

        // `responseMimeType` / `responseSchema` are typed fields now. Promote
        // `responseSchema` (paired with `responseMimeType: "application/json"`)
        // into the canonical slot so cross-protocol routing can translate it;
        // any other MIME mode (e.g. `text/x.enum`) re-attaches to extras as an
        // opaque Google-native pass-through.
        let (mut params, response_format) = match req.generation_config {
            Some(g) => {
                let GenerateContentGenerationConfig {
                    temperature,
                    top_p,
                    max_output_tokens,
                    response_mime_type,
                    response_schema,
                    top_k,
                    seed,
                    stop_sequences,
                    presence_penalty,
                    frequency_penalty,
                    mut extra,
                } = g;
                let response_format = match (
                    response_mime_type.as_deref() == Some("application/json"),
                    response_schema,
                ) {
                    (true, Some(schema)) => Some(ResponseFormat::JsonSchema {
                        name: None,
                        description: None,
                        strict: None,
                        schema,
                    }),
                    // Not a JSON-schema constraint — re-attach for opaque passthrough.
                    (_, schema) => {
                        if let Some(mime) = response_mime_type {
                            extra.insert("responseMimeType".to_string(), mime.into());
                        }
                        if let Some(schema) = schema {
                            extra.insert("responseSchema".to_string(), schema);
                        }
                        None
                    }
                };
                let response_modalities = take_gemini_modalities(&mut extra);
                (
                    GenerationParams {
                        temperature,
                        top_p,
                        max_tokens: max_output_tokens,
                        reasoning_effort: None,
                        response_modalities,
                        top_k,
                        seed,
                        stop: stop_sequences.unwrap_or_default(),
                        presence_penalty,
                        frequency_penalty,
                        extra,
                    },
                    response_format,
                )
            }
            None => (GenerationParams::default(), None),
        };
        // Promote `toolConfig.functionCallingConfig` into the canonical
        // tool_choice slot so it translates across protocols; the rest of
        // `toolConfig` (and other top-level extras) still ride through.
        let tool_choice = parse_gc_tool_choice(&mut req.extra);
        // Preserve top-level Google fields (`toolConfig`, `safetySettings`,
        // `cachedContent`, …) across the round-trip. They're namespaced so they
        // don't collide with `generationConfig`-level extras above and only the
        // Generate Content `render_request` lifts them back to the top level.
        if !req.extra.is_empty() {
            params.extra.insert(
                GOOGLE_TOP_LEVEL_EXTRA_KEY.to_string(),
                serde_json::Value::Object(req.extra.into_iter().collect()),
            );
        }

        Ok(Prompt {
            model: req.model,
            system: system.filter(|s| !s.is_empty()),
            // Generate Content has no system-level `cache_control` on its wire.
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools,
            params,
            response_format,
            tool_choice,
            stream: req.stream,
        })
    }

    fn render_response(
        &self,
        result: &GenerateResult,
        _prompt: &Prompt,
        _request_id: &str,
    ) -> Result<serde_json::Value> {
        let parts: Vec<serde_json::Value> = result.content.iter().filter_map(render_part).collect();
        let usage = result.usage.unwrap_or_default();
        let mut candidate = serde_json::json!({
            "content": { "role": "model", "parts": parts },
            // Prefer the stashed raw `finishReason` (e.g. `RECITATION`) over the
            // unified-enum mapping so a same-protocol round-trip is byte-faithful;
            // default to `STOP` when the result carries no finish reason at all.
            "finishReason": rendered_finish_reason(result, PROVIDER_ID_GOOGLE, finish_reason_str)
                .unwrap_or_else(|| "STOP".to_string()),
            "index": 0,
        });
        // Re-attach web-search citations under `candidate.groundingMetadata`
        // (the location `parse_response` lifts them from), collected from the
        // result's `Content::Source` parts rather than rendered into `parts`
        // (grounding is candidate metadata, not a content part).
        // <https://ai.google.dev/gemini-api/docs/grounding>
        let chunks = render_grounding_chunks(result);
        if !chunks.is_empty() {
            candidate["groundingMetadata"] = serde_json::json!({ "groundingChunks": chunks });
        }
        let mut body = serde_json::json!({
            "candidates": [candidate],
            "usageMetadata": {
                "promptTokenCount": usage.prompt_tokens,
                "candidatesTokenCount": usage.completion_tokens,
                "totalTokenCount": usage.total(),
                "thoughtsTokenCount": usage.reasoning_tokens,
            },
        });
        // Restore the result-level `modelVersion` when it round-tripped (only
        // ever set by this protocol's `parse_response`).
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        if let Some(mv) = provider_namespace(&result.provider_metadata, PROVIDER_ID_GOOGLE)
            .and_then(|o| o.get(GOOGLE_MODEL_VERSION))
        {
            body["modelVersion"] = mv.clone();
        }
        Ok(body)
    }

    fn stream_encoder(&self, _request_id: &str, _model: &str) -> Box<dyn StreamEncoder> {
        Box::new(GenerateContentStreamEncoder::default())
    }
}

impl OutboundAdapter for GenerateContentAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::GenerateContent
    }

    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value> {
        let contents: Vec<serde_json::Value> = prompt.messages.iter().map(render_message).collect();
        let mut req = serde_json::Map::new();
        req.insert("contents".into(), contents.into());
        if let Some(system) = &prompt.system {
            req.insert(
                "systemInstruction".into(),
                serde_json::json!({ "parts": [{ "text": system }] }),
            );
        }
        if !prompt.tools.is_empty() {
            req.insert("tools".into(), render_generate_content_tools(&prompt.tools));
        }
        let mut gen_config = serde_json::Map::new();
        if let Some(t) = prompt.params.temperature {
            gen_config.insert("temperature".into(), t.into());
        }
        if let Some(p) = prompt.params.top_p {
            gen_config.insert("topP".into(), p.into());
        }
        if let Some(mt) = prompt.params.max_tokens {
            gen_config.insert("maxOutputTokens".into(), mt.into());
        }
        // Render the canonical response_format into Google's generationConfig.
        // `name` and `strict` are intentionally dropped — Google's
        // schema-constrained sampling has no concept of either.
        if let Some(rf) = &prompt.response_format {
            let ResponseFormat::JsonSchema { schema, .. } = rf;
            gen_config.insert("responseMimeType".into(), "application/json".into());
            gen_config.insert("responseSchema".into(), schema.clone());
        }
        // Output modalities -> Google `responseModalities` (uppercase tokens).
        if !prompt.params.response_modalities.is_empty() {
            let tokens: Vec<serde_json::Value> = prompt
                .params
                .response_modalities
                .iter()
                .map(|m| modality_to_gemini(m).into())
                .collect();
            gen_config.insert("responseModalities".into(), tokens.into());
        }
        // Render the typed sampling slots into their nested `generationConfig`
        // wire names. Gemini carries all five, so a `stop` authored on a Chat
        // client reaches here as `stopSequences`, an Anthropic `top_k` as `topK`,
        // and so on.
        // <https://ai.google.dev/api/generate-content#GenerationConfig>
        if let Some(top_k) = prompt.params.top_k {
            gen_config.insert("topK".into(), top_k.into());
        }
        if let Some(seed) = prompt.params.seed {
            gen_config.insert("seed".into(), seed.into());
        }
        if !prompt.params.stop.is_empty() {
            gen_config.insert("stopSequences".into(), prompt.params.stop.clone().into());
        }
        if let Some(pp) = prompt.params.presence_penalty {
            gen_config.insert("presencePenalty".into(), pp.into());
        }
        if let Some(fp) = prompt.params.frequency_penalty {
            gen_config.insert("frequencyPenalty".into(), fp.into());
        }
        // Splat remaining Google generation-config extras (responseLogprobs,
        // candidateCount, …) back into the outbound config. Typed fields above
        // win over a same-named extra; the sentinel key carries top-level fields
        // and is skipped here.
        for (k, v) in &prompt.params.extra {
            if k == GOOGLE_TOP_LEVEL_EXTRA_KEY {
                continue;
            }
            gen_config.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if !gen_config.is_empty() {
            req.insert("generationConfig".into(), gen_config.into());
        }
        // Lift namespaced top-level extras (safetySettings / cachedContent / …)
        // back to the request root.
        if let Some(serde_json::Value::Object(top)) =
            prompt.params.extra.get(GOOGLE_TOP_LEVEL_EXTRA_KEY)
        {
            for (k, v) in top {
                req.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        // Render the canonical tool_choice into Google's
        // `toolConfig.functionCallingConfig`, merging into any lifted `toolConfig`
        // (e.g. a passed-through `retrievalConfig`) and overriding its
        // function-calling config so the canonical slot wins.
        if let Some(tc) = &prompt.tool_choice {
            let tool_config = req
                .entry("toolConfig".to_string())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = tool_config.as_object_mut() {
                obj.insert(
                    "functionCallingConfig".into(),
                    render_gc_function_calling_config(tc),
                );
            }
        }
        Ok(serde_json::Value::Object(req))
    }

    fn parse_response(&self, body: serde_json::Value) -> Result<GenerateResult> {
        let candidate = body
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| {
                BitrouterError::bad_request("google response missing 'candidates[0]'")
            })?;
        let mut parts = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|p| parse_parts(p))
            .unwrap_or_default();
        // Web-search grounding rides `candidate.groundingMetadata`, separate
        // from the content `parts`. Lift its `groundingChunks` into
        // `Content::Source` parts appended after the content.
        // <https://ai.google.dev/gemini-api/docs/grounding>
        parts.extend(parse_grounding_sources(candidate.get("groundingMetadata")));
        let raw_finish = candidate
            .get("finishReason")
            .and_then(|f| f.as_str())
            .map(str::to_string);
        let finish = raw_finish.as_deref().and_then(finish_reason);
        let usage = body.get("usageMetadata").and_then(parse_usage);
        // Generate Content: top-level `responseId`.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        let response_id = body
            .get("responseId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Gemini's `modelVersion` (the exact model build that served the
        // response) has no dedicated canonical field — carry it at result level
        // under the `google` namespace.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        let mut provider_metadata = ProviderMetadata::new();
        if let Some(mv) = body.get("modelVersion").filter(|v| !v.is_null()) {
            set_provider_metadata(
                &mut provider_metadata,
                PROVIDER_ID_GOOGLE,
                GOOGLE_MODEL_VERSION,
                mv.clone(),
            );
        }
        // Preserve the raw `finishReason` when the unified enum can't reproduce
        // it: the content-filter family (`RECITATION` / `BLOCKLIST` /
        // `PROHIBITED_CONTENT`) all collapse to `ContentFilter`, which renders
        // back as the canonical `SAFETY`, so the precise sub-reason would be
        // lost without stashing it under the `google` namespace.
        stash_raw_finish_reason(
            &mut provider_metadata,
            PROVIDER_ID_GOOGLE,
            raw_finish.as_deref(),
            finish.as_ref(),
            finish_reason_str,
        );
        Ok(GenerateResult {
            content: parts,
            usage,
            finish_reason: finish,
            response_id,
            stop_details: None,
            provider_metadata,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GenerateContentStreamDecoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

#[async_trait]
impl Transport for GenerateContentTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::GenerateContent
    }

    fn endpoint_url(&self, target: &RoutingTarget, stream: bool) -> String {
        let base = target.effective_api_base().trim_end_matches('/');
        let verb = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        format!("{base}/models/{}:{verb}", target.service_id)
    }

    async fn authorise(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let key = target.effective_api_key();
        let header = reqwest::header::HeaderValue::from_str(key).map_err(|e| {
            BitrouterError::internal(format!("invalid api key for x-goog-api-key header: {e}"))
        })?;
        request.headers_mut().insert("x-goog-api-key", header);
        Ok(request)
    }
}

fn render_part(c: &Content) -> Option<serde_json::Value> {
    match c {
        Content::Text { text, .. } => Some(serde_json::json!({ "text": text })),
        Content::Reasoning {
            text,
            provider_metadata,
        } => {
            let mut part = serde_json::json!({ "text": text, "thought": true });
            // Restore the thinking part's `thoughtSignature` continuity token.
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        Content::ToolCall {
            name,
            arguments,
            provider_metadata,
            ..
        } => {
            let args: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
            let mut part = serde_json::json!({ "functionCall": { "name": name, "args": args } });
            // A `functionCall` continuing a reasoning chain carries a
            // `thoughtSignature`; restore it when it round-tripped.
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        // Gemini `functionResponse {id?, name, response}`: `response` must be a
        // JSON object and there is no error flag or media slot. `name` comes from
        // `tool_name` (Gemini keys results by name), falling back to `call_id`;
        // `id` rides along when it differs from the name. A non-object output
        // degrades losslessly under a `result` key.
        //
        // Known degrade: a multimodal `Content` output collapses to
        // `{ result: <concatenated text> }` here — its media and provider
        // file-reference parts are dropped. The V3 `FunctionResponse.parts[]`
        // array (which CAN carry inline media alongside the response) is left
        // unused; modeling it is deferred until a consumer needs Gemini-side tool
        // media, since today no other request wire round-trips media *out* of a
        // tool result into Gemini.
        // <https://ai.google.dev/api/caching#FunctionResponse>
        Content::ToolResult {
            call_id,
            tool_name,
            output,
            ..
        } => {
            let name = tool_name.as_deref().unwrap_or(call_id);
            let response = match output {
                ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value }
                    if value.is_object() =>
                {
                    value.clone()
                }
                ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
                    serde_json::json!({ "result": value })
                }
                other => serde_json::json!({ "result": other.to_provider_string() }),
            };
            let mut fr = serde_json::json!({ "name": name, "response": response });
            // Carry the call id only when it adds information beyond the name.
            if call_id != name && !call_id.is_empty() {
                fr["id"] = serde_json::Value::String(call_id.clone());
            }
            Some(serde_json::json!({ "functionResponse": fr }))
        }
        // Inline bytes -> `inlineData`; a URL -> `fileData`. Gemini keys media by
        // `mimeType`. <https://ai.google.dev/gemini-api/docs/image-understanding>
        Content::File {
            media_type, data, ..
        } => Some(match data {
            DataContent::Base64 { data } => serde_json::json!({
                "inlineData": { "mimeType": media_type, "data": data }
            }),
            DataContent::Url { url } => serde_json::json!({
                "fileData": { "mimeType": media_type, "fileUri": url }
            }),
        }),
        // Sources are response-side citation metadata, never a request part —
        // they are re-attached under `groundingMetadata` in `render_response`,
        // not rendered as a content `part`. Skip on the request path.
        Content::Source { .. } => None,
        // The Gemini Generate Content wire has no tool-approval handshake: there
        // is no `mcp_approval_request` / `mcp_approval_response` part. The AI
        // SDK's Google converter drops a `tool-approval-response` part
        // (`continue`), so both approval parts are skipped here. A denied
        // execution degrades to a `functionResponse {result: <denial string>}`
        // via the `ToolResult` arm above (`to_provider_string`).
        // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/google/src/convert-to-google-generative-ai-messages.ts>
        Content::ToolApprovalRequest { .. } | Content::ToolApprovalResponse { .. } => None,
    }
}

fn render_message(m: &Message) -> serde_json::Value {
    let parts: Vec<serde_json::Value> = m.content.iter().filter_map(render_part).collect();
    serde_json::json!({ "role": role_str(m.role), "parts": parts })
}

fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    // Absence of `promptTokenCount` means the chunk carries no usage at all.
    let prompt = value.get("promptTokenCount")?.as_u64().unwrap_or(0);
    let candidates = value
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = value
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Gemini reports cached prompt tokens under `cachedContentTokenCount`
    // (ai.google.dev/api/generate-content). No write-side counter is exposed.
    let cache_read = value
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: candidates,
        reasoning_tokens: reasoning,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
    })
}

// ===== streaming =====

/// Generate Content `streamGenerateContent` SSE decoder. Each `data:` line is a full
/// `GenerateContentResponse` with partial candidates.
#[derive(Default)]
struct GenerateContentStreamDecoder {
    finished: bool,
    /// Whether the one-shot [`StreamPart::ResponseStarted`] has been emitted.
    /// Every chunk repeats `responseId`; we surface it only once.
    response_started_emitted: bool,
    /// Dedup keys of grounding sources already emitted as `StreamPart::Source`
    /// — the citation URL for a [`Source::Url`], the synthesized id for a
    /// [`Source::Document`]. `streamGenerateContent` repeats the accumulating
    /// `groundingMetadata` on successive chunks, so dedupe to emit each citation
    /// once — matching the AI SDK's `emittedSourceUrls` set.
    /// <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/google/src/google-generative-ai-language-model.ts>
    emitted_source_keys: std::collections::HashSet<String>,
}

impl StreamDecoder for GenerateContentStreamDecoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let chunk: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let mut parts = Vec::new();
        // Surface the upstream response id once. Every streamGenerateContent
        // chunk repeats top-level `responseId`; emit it a single time for
        // observability.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        if !self.response_started_emitted
            && let Some(id) = chunk
                .get("responseId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        {
            self.response_started_emitted = true;
            parts.push(StreamPart::ResponseStarted { id: id.to_string() });
        }
        if let Some(candidate) = chunk
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        {
            if let Some(content_parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for content in parse_parts(content_parts) {
                    match content {
                        Content::Text { text, .. } => parts.push(StreamPart::TextDelta { text }),
                        Content::Reasoning { text, .. } => {
                            parts.push(StreamPart::ReasoningDelta { text })
                        }
                        // The streaming `ToolCallDelta` has no provider-executed
                        // flag (Gemini streams only client `functionCall` parts),
                        // so the server-tool marker is not carried here — `..`
                        // ignores it deliberately.
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => parts.push(StreamPart::ToolCallDelta {
                            id,
                            name: Some(name),
                            arguments,
                        }),
                        Content::ToolResult { .. } => {}
                        // A generated file (e.g. an image) becomes one whole
                        // `StreamPart::File`, matching the AI SDK V3 stream `file`
                        // part. <https://ai.google.dev/gemini-api/docs/image-generation>
                        Content::File {
                            media_type, data, ..
                        } => parts.push(StreamPart::File { media_type, data }),
                        // `parse_parts` never produces `Source` (grounding is
                        // candidate metadata, not a content part); it is decoded
                        // from `groundingMetadata` below.
                        Content::Source { .. } => {}
                        // The Gemini wire carries no tool-approval handshake, so
                        // `parse_parts` never yields these; nothing to stream.
                        Content::ToolApprovalRequest { .. }
                        | Content::ToolApprovalResponse { .. } => {}
                    }
                }
            }
            // Web-search grounding arrives on `candidate.groundingMetadata`,
            // accumulating across chunks. Emit each new citation once as a whole
            // `StreamPart::Source`, deduped by URL (or document id). Every
            // grounding chunk kind `parse_grounding_sources` recognizes — URL
            // and document sources alike — is forwarded; none is silently
            // dropped here.
            // <https://ai.google.dev/gemini-api/docs/grounding>
            for content in parse_grounding_sources(candidate.get("groundingMetadata")) {
                if let Content::Source { source, .. } = content {
                    let key = match &source {
                        Source::Url { url, .. } => url.clone(),
                        Source::Document { id, .. } => id.clone(),
                    };
                    if self.emitted_source_keys.insert(key) {
                        parts.push(StreamPart::Source { source });
                    }
                }
            }
            if let Some(reason) = candidate
                .get("finishReason")
                .and_then(|f| f.as_str())
                .and_then(finish_reason)
            {
                if let Some(usage) = chunk.get("usageMetadata").and_then(parse_usage) {
                    parts.push(StreamPart::Usage { usage });
                }
                parts.push(StreamPart::Finish { reason });
                self.finished = true;
            }
        }
        Ok(parts)
    }
}

/// Generate Content `streamGenerateContent` SSE encoder — most canonical parts
/// become one `GenerateContentResponse` chunk each.
///
/// Tool calls are the exception: Generate Content has no incremental
/// tool-argument frame — a `functionCall` part carries the whole `{name, args}`
/// at once. So unlike Chat Completions / Responses (which stream argument
/// deltas), this encoder must BUFFER a tool call and emit it as one chunk once
/// complete. Accumulating also makes it robust to upstreams that re-send
/// `name:""` on every continuation chunk (which would otherwise emit one broken
/// `functionCall` per fragment).
#[derive(Default)]
struct GenerateContentStreamEncoder {
    /// `(name, accumulated raw-JSON arguments)` of the tool call awaiting emission.
    pending_tool: Option<(String, String)>,
}

impl GenerateContentStreamEncoder {
    /// Emit the buffered tool call, if any, as one `functionCall` chunk.
    fn flush_pending_tool(&mut self) -> Option<serde_json::Value> {
        let (name, args) = self.pending_tool.take()?;
        let args: serde_json::Value =
            serde_json::from_str(if args.is_empty() { "{}" } else { &args })
                .unwrap_or_else(|_| serde_json::json!({}));
        Some(serde_json::json!({
            "candidates": [{ "content": { "role": "model", "parts": [{
                "functionCall": { "name": name, "args": args }
            }] } }]
        }))
    }

    fn to_frame(chunk: serde_json::Value) -> SseFrame {
        SseFrame::Event {
            event: None,
            data: chunk.to_string(),
        }
    }
}

impl StreamEncoder for GenerateContentStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let mut chunks: Vec<serde_json::Value> = Vec::new();
        match part {
            // Observability-only metadata (upstream response id) — never
            // forwarded to the Google-protocol client.
            StreamPart::ResponseStarted { .. } => return Ok(Vec::new()),
            // Coarse wire: Generate Content frames no content blocks (text /
            // reasoning are flat `parts` on one candidate), so block-lifecycle
            // markers have no native chunk and re-encode to nothing.
            StreamPart::TextStart { .. }
            | StreamPart::TextEnd { .. }
            | StreamPart::ReasoningStart { .. }
            | StreamPart::ReasoningEnd { .. } => return Ok(Vec::new()),
            // A tool call buffers until complete (see struct docs). A delta with
            // a *non-empty* name starts a new call (flush the previous one); an
            // empty/absent name is an argument continuation of the open call —
            // some upstreams re-send `name:""` on every chunk, which must NOT be
            // read as a new call.
            StreamPart::ToolCallDelta {
                name, arguments, ..
            } => match name.as_deref().filter(|n| !n.is_empty()) {
                Some(name) => {
                    if let Some(c) = self.flush_pending_tool() {
                        chunks.push(c);
                    }
                    self.pending_tool = Some((name.to_string(), arguments.clone()));
                }
                None => match self.pending_tool.as_mut() {
                    Some((_, acc)) => acc.push_str(arguments),
                    None => self.pending_tool = Some((String::new(), arguments.clone())),
                },
            },
            // Every other emitting part flushes the buffered tool call first, so
            // a completed `functionCall` is never stranded behind later content.
            // A generated file -> an `inlineData` / `fileData` part in one chunk.
            // <https://ai.google.dev/gemini-api/docs/image-generation>
            StreamPart::File { media_type, data } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                let file_part = match data {
                    DataContent::Base64 { data } => serde_json::json!({
                        "inlineData": { "mimeType": media_type, "data": data }
                    }),
                    DataContent::Url { url } => serde_json::json!({
                        "fileData": { "mimeType": media_type, "fileUri": url }
                    }),
                };
                chunks.push(serde_json::json!({
                    "candidates": [{ "content": { "role": "model", "parts": [file_part] } }]
                }));
            }
            // Generate Content has no server-tool / MCP stream form; emitting a
            // functionCall/functionResponse here would look to the client like a
            // pending call it must answer. Router-executed tool activity is
            // dropped on this wire (the model's narration + final answer still
            // stream); flush any buffered ordinary tool call first. Documented
            // limitation.
            StreamPart::ServerToolCall { .. } | StreamPart::ServerToolResult { .. } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
            }
            StreamPart::TextDelta { text } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                chunks.push(serde_json::json!({
                    "candidates": [{ "content": { "role": "model", "parts": [{ "text": text }] } }]
                }));
            }
            StreamPart::ReasoningDelta { text } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                chunks.push(serde_json::json!({
                    "candidates": [{ "content": { "role": "model",
                        "parts": [{ "text": text, "thought": true }] } }]
                }));
            }
            // Re-attach a streamed citation as a one-chunk
            // `candidate.groundingMetadata.groundingChunks` web entry — the
            // location the decoder reads. Only a URL source maps; a document
            // citation is dropped (no `groundingChunks.web` form on this wire).
            // <https://ai.google.dev/api/generate-content#GroundingChunk>
            StreamPart::Source { source } => match source {
                Source::Url { url, title, .. } => {
                    if let Some(c) = self.flush_pending_tool() {
                        chunks.push(c);
                    }
                    let mut web = serde_json::Map::new();
                    web.insert("uri".into(), url.clone().into());
                    if let Some(title) = title {
                        web.insert("title".into(), title.clone().into());
                    }
                    chunks.push(serde_json::json!({
                        "candidates": [{
                            "content": { "role": "model", "parts": [] },
                            "groundingMetadata": {
                                "groundingChunks": [{ "web": serde_json::Value::Object(web) }]
                            },
                        }]
                    }));
                }
                // No `groundingChunks.web` form for a document citation.
                Source::Document { .. } => return Ok(Vec::new()),
            },
            StreamPart::Usage { usage } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                chunks.push(serde_json::json!({
                    "usageMetadata": {
                        "promptTokenCount": usage.prompt_tokens,
                        "candidatesTokenCount": usage.completion_tokens,
                        "totalTokenCount": usage.total(),
                        "thoughtsTokenCount": usage.reasoning_tokens,
                    }
                }));
            }
            StreamPart::Finish { reason } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                chunks.push(serde_json::json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [] },
                        "finishReason": finish_reason_str(reason),
                    }]
                }));
            }
            StreamPart::ResponseCompleted { status, usage, .. } => {
                if let Some(c) = self.flush_pending_tool() {
                    chunks.push(c);
                }
                // Inbound was Responses; Generate Content has no response-completed
                // concept — emit a terminal candidate with a mapped
                // `finishReason`, plus `usageMetadata` if usage was carried.
                let finish_reason = if status == "incomplete" {
                    "MAX_TOKENS"
                } else {
                    "STOP"
                };
                let mut chunk = serde_json::json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [] },
                        "finishReason": finish_reason,
                    }]
                });
                if let Some(u) = usage {
                    chunk["usageMetadata"] = serde_json::json!({
                        "promptTokenCount": u.prompt_tokens,
                        "candidatesTokenCount": u.completion_tokens,
                        "totalTokenCount": u.total(),
                        "thoughtsTokenCount": u.reasoning_tokens,
                    });
                }
                chunks.push(chunk);
            }
        };
        Ok(chunks.into_iter().map(Self::to_frame).collect())
    }

    /// Flush any tool call still buffered when the stream ends without a
    /// trailing `Finish` / `ResponseCompleted` part.
    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        Ok(self
            .flush_pending_tool()
            .map(Self::to_frame)
            .into_iter()
            .collect())
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Generate Content surfaces a mid-stream error as a chunk carrying an `error`
        // object (mirrors the non-streaming error envelope). Any buffered tool call
        // is intentionally dropped — a partial `functionCall` must not be emitted
        // alongside an error.
        vec![SseFrame::Event {
            event: None,
            data: serde_json::json!({
                "error": { "code": 502, "status": "UNAVAILABLE", "message": message }
            })
            .to_string(),
        }]
    }
}
