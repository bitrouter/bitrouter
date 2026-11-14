//! Generate Content (`generateContent`) adapter.
//!
//! Official reference: <https://ai.google.dev/api/generate-content>
//! Streaming: <https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent>
//!
//! Google uses `contents[]` with roles `user` / `model`, a separate
//! `systemInstruction`, and `parts[]` of `{text}` / `{functionCall}` /
//! `{functionResponse}`. Reasoning is a `part` flagged `thought: true`
//! Signatures remain native continuity metadata, not cross-provider authority.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    InboundAdapter, OutboundAdapter, StreamDecoder, StreamEncoder, rendered_finish_reason,
    stash_raw_finish_reason,
    values::{extra_value_for_protocol, extras_for_protocol, synthesize_source_id},
};
use crate::gateway::model::error::{ModelError, Result};
use bitrouter_ai::types::{
    ApiProtocol, Content, DataContent, FinishReason, GenerateResult, GenerationParams, Message,
    Modality, Prompt, ProviderMetadata, ReasoningEffort, ResponseFormat, Role, Source, Tool,
    ToolChoice, ToolResultOutput, Usage, UsageOrigin, provider_namespace, set_provider_metadata,
};

pub(crate) mod native;
pub(crate) mod paired;
mod parts;
mod stream;

const PROVIDER_ID_GOOGLE: &str = "google";

fn protocol() -> ApiProtocol {
    ApiProtocol::Custom("generate_content".into())
}

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

/// Restricted Generate Content canonical projection, without route registration.
///
/// Unsupported native parts and foreign task/continuity constraints refuse.
/// This is not an authenticated native replay owner: callers must retain exact
/// native bytes separately. Parts/stream accumulation is bounded to 1 MiB and
/// 64 client calls and 4096 native parts. Signed stream text/media needs native wire custody because
/// the canonical delta types cannot retain its metadata.
pub struct GenerateContentAdapter;

/// Sentinel key under which top-level Google extras (`toolConfig`,
/// `safetySettings`, `cachedContent`, …) ride through `GenerationParams::extra`.
/// Native rendering lifts these fields to their original location. Ordinary
/// target admission refuses untranslatable controls before any provider send.
const GOOGLE_TOP_LEVEL_EXTRA_KEY: &str = "__google_top_level__";

// ===== wire request types =====

/// Generate Content request body after the ingress path supplies model and mode.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest {
    /// Model supplied by the owner of the private protocol envelope.
    #[serde(default)]
    model: String,
    contents: Vec<GenerateContentContent>,
    #[serde(default)]
    system_instruction: Option<GenerateContentContent>,
    #[serde(default)]
    tools: Vec<GenerateContentTool>,
    #[serde(default)]
    generation_config: Option<GenerateContentGenerationConfig>,
    /// Streaming mode supplied by the ingress owner; this codec mounts no route.
    #[serde(default)]
    stream: bool,
    /// Top-level extras: `toolConfig`, `safetySettings`, `cachedContent`, … —
    /// preserve them across the inbound→outbound round-trip. Per
    /// <https://ai.google.dev/api/generate-content>. Retain them separately from
    /// the canonical sampling fields for native rendering and target admission.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// One element of [`GenerateContentRequest`]'s `contents` array — a turn
/// carrying optional role + `parts[]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateContentContent {
    #[serde(default)]
    role: Option<String>,
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
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentTool {
    #[serde(default)]
    function_declarations: Vec<GenerateContentFunctionDecl>,
    /// Provider-defined (built-in) tool keys on this object — `googleSearch`,
    /// `codeExecution`, `googleSearchRetrieval`, `urlContext`, … — each preserved
    /// verbatim; foreign providers must not interpret them as client tools.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// One function declaration inside a [`GenerateContentTool`]: name + description +
/// JSON-Schema parameters.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateContentFunctionDecl {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
}

/// `generationConfig` knobs on a [`GenerateContentRequest`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentGenerationConfig {
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
    /// Qualitative reasoning effort for Gemini 3+ models. Numeric
    /// `thinkingBudget` remains an opaque sibling because it is a distinct
    /// Gemini 2.5 control with no lossless qualitative mapping.
    #[serde(default)]
    thinking_config: Option<GenerateContentThinkingConfig>,
    /// `responseLogprobs`, `candidateCount`, `thinkingConfig`, … — every
    /// generation-config knob without a typed slot rides via `extra` and is
    /// retained separately in `extra`. Rendering refuses unknown fields rather
    /// than dropping them. Untranslated controls exclude ordinary targets.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Gemini `generationConfig.thinkingConfig`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentThinkingConfig {
    #[serde(default)]
    thinking_level: Option<ReasoningEffort>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

fn invalid_response(message: &'static str) -> ModelError {
    ModelError::InvalidResponse {
        message: message.into(),
        usage: None,
    }
}

/// Map a Google role to its canonical role.
fn parse_role(role: Option<&str>) -> Result<Role> {
    match role {
        Some("user") | None => Ok(Role::User),
        Some("model") => Ok(Role::Assistant),
        Some("function") | Some("tool") => Ok(Role::Tool),
        Some(other) => Err(ModelError::invalid_request(format!(
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
    let Some(tokens) = extra.get("responseModalities").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(modalities) = tokens
        .iter()
        .map(|token| token.as_str().and_then(modality_from_gemini))
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    extra.remove("responseModalities");
    modalities
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
/// Native tool schemas are retained exactly; unsupported strict declarations
/// and foreign provider-defined tools refuse instead of weakening constraints.
/// <https://ai.google.dev/gemini-api/docs/function-calling>
/// <https://ai.google.dev/gemini-api/docs/google-search>
fn render_generate_content_tools(tools: &[Tool]) -> Result<Value> {
    let mut function_declarations = Vec::new();
    let mut entries = Vec::new();
    for tool in tools {
        match tool {
            Tool::Function {
                name,
                description,
                parameters,
                strict,
                ..
            } => {
                if strict.is_some() {
                    return Err(ModelError::invalid_request(
                        "strict tool declarations have no GenerateContent projection",
                    ));
                }
                function_declarations.push(serde_json::json!({
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                }));
            }
            Tool::ProviderDefined { id, args, .. } => {
                let name = id
                    .strip_prefix("google.")
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ModelError::invalid_request(
                            "foreign native tool cannot be rendered as GenerateContent",
                        )
                    })?;
                let args = args.as_object().ok_or_else(|| {
                    ModelError::invalid_request(
                        "GenerateContent native tool arguments must be an object",
                    )
                })?;
                entries.push(json!({ name: args }));
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
    Ok(Value::Array(out))
}

/// The fields Gemini's `GenerationConfig` declares — the extras splat on
/// render forwards only these (the wire rejects unknown names with a 400).
/// <https://ai.google.dev/api/generate-content#GenerationConfig>
const GEMINI_GENERATION_CONFIG_KEYS: &[&str] = &[
    "stopSequences",
    "responseMimeType",
    "responseSchema",
    "responseJsonSchema",
    "responseModalities",
    "candidateCount",
    "maxOutputTokens",
    "temperature",
    "topP",
    "topK",
    "seed",
    "presencePenalty",
    "frequencyPenalty",
    "responseLogprobs",
    "logprobs",
    "enableEnhancedCivicAnswers",
    "speechConfig",
    "thinkingConfig",
    "imageConfig",
    "mediaResolution",
    "audioTimestamp",
];

/// Restore an opaque signature only to its native part. The authenticated
/// provider owner, not this metadata string, establishes continuity authority.
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
fn parse_parts(parts: &[Value]) -> Result<Vec<Content>> {
    parts::Parts::default().decode(parts)
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
/// metadata is outside this restricted canonical citation projection. Exact
/// native responses must retain their original grounding object separately.
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
            id: synthesize_source_id(&url, index),
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
                    id: synthesize_source_id(uri, index),
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
                    id: synthesize_source_id(store, index),
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
/// document citations are refused by `render_parts` before this helper runs.
/// Returns an empty Vec when the result carries no URL sources.
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
        "" | "FINISH_REASON_UNSPECIFIED" => None,
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
    let mode = fcc.get("mode").and_then(|m| m.as_str()).map(str::to_owned);
    let names: Vec<String> = match fcc.get("allowedFunctionNames") {
        None => Vec::new(),
        Some(Value::Array(values)) => {
            let names = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()?;
            if names.iter().collect::<std::collections::HashSet<_>>().len() != names.len() {
                return None;
            }
            names
        }
        _ => return None,
    };
    if fcc
        .keys()
        .any(|key| !matches!(key.as_str(), "mode" | "allowedFunctionNames"))
    {
        return None;
    }
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
        protocol()
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        bitrouter_ai::wire::json(&body, 16 * 1024 * 1024).map_err(|_| {
            ModelError::invalid_request("GenerateContent request exceeds codec bound")
        })?;
        let mut req: GenerateContentRequest = serde_json::from_value(body)
            .map_err(|_| ModelError::invalid_request("invalid GenerateContent request shape"))?;

        if req.contents.is_empty() {
            return Err(ModelError::invalid_request(
                "GenerateContent contents must not be empty",
            ));
        }
        let system = req
            .system_instruction
            .as_ref()
            .map(|instruction| {
                instruction
                    .parts
                    .iter()
                    .map(|part| {
                        let object = part.as_object().ok_or_else(|| {
                            ModelError::invalid_request("invalid native system instruction")
                        })?;
                        if object.len() != 1 {
                            return Err(ModelError::invalid_request(
                                "unsupported native system metadata",
                            ));
                        }
                        object.get("text").and_then(Value::as_str).ok_or_else(|| {
                            ModelError::invalid_request("system instruction requires text")
                        })
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(|parts| parts.join("\n"))
            })
            .transpose()?;

        let mut messages = Vec::with_capacity(req.contents.len());
        let mut native_parts = parts::Parts::default();
        for c in &req.contents {
            let role = parse_role(c.role.as_deref())?;
            let parsed = native_parts.decode(&c.parts).map_err(|_| {
                ModelError::invalid_request("unsupported GenerateContent request part")
            })?;
            let (tool_results, rest): (Vec<_>, Vec<_>) = parsed
                .into_iter()
                .partition(|x| matches!(x, Content::ToolResult { .. }));
            if !tool_results.is_empty() && !rest.is_empty() {
                return Err(ModelError::invalid_request(
                    "mixed tool-result and ordinary native parts cannot preserve order",
                ));
            }
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
                    thinking_config,
                    mut extra,
                } = g;
                // A config extension cannot impersonate a root field.
                if extra.contains_key(GOOGLE_TOP_LEVEL_EXTRA_KEY) {
                    return Err(ModelError::invalid_request(
                        "reserved GenerateContent config key",
                    ));
                }
                let reasoning_effort = match thinking_config {
                    Some(thinking) => {
                        let GenerateContentThinkingConfig {
                            thinking_level,
                            extra: thinking_extra,
                        } = thinking;
                        if thinking_level.is_some() && thinking_extra.contains_key("thinkingBudget")
                        {
                            return Err(ModelError::invalid_request(
                                "Gemini thinkingLevel and thinkingBudget cannot be used together",
                            ));
                        }
                        if !thinking_extra.is_empty() {
                            extra.insert(
                                "thinkingConfig".to_string(),
                                serde_json::Value::Object(thinking_extra.into_iter().collect()),
                            );
                        }
                        thinking_level
                    }
                    None => None,
                };
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
                        chat_token_limit_field: None,
                        reasoning_effort,
                        reasoning_effort_source: Default::default(),
                        response_modalities,
                        top_k,
                        seed,
                        stop: stop_sequences.unwrap_or_default(),
                        presence_penalty,
                        frequency_penalty,
                        store: None,
                        parallel_tool_calls: None,
                        chat_stream_options: None,
                        extra_protocol: Some(protocol()),
                        supplemental_extra: std::collections::HashMap::new(),
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
        params.extra_protocol = Some(protocol());

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
    ) -> Result<Value> {
        if result.content.iter().any(|part| {
            matches!(
                part,
                Content::Reasoning {
                    native: Some(_),
                    ..
                }
            )
        }) {
            return Err(invalid_response(
                "native reasoning cannot be represented in GenerateContent",
            ));
        }
        if result
            .content
            .iter()
            .any(|part| matches!(part, Content::ToolCall { .. }))
            && !matches!(
                result.finish_reason,
                Some(FinishReason::Stop | FinishReason::ToolCalls)
            )
        {
            return Err(invalid_response(
                "native function calls require a complete source turn",
            ));
        }
        if matches!(result.finish_reason, Some(FinishReason::Error(_))) {
            return Err(invalid_response(
                "unsuccessful source response has no successful native projection",
            ));
        }
        let parts = render_parts(&result.content)?;
        let usage = result.usage.as_ref().map(render_usage).transpose()?;
        let mut candidate = serde_json::json!({
            "content": { "role": "model", "parts": parts },
            // Prefer the stashed raw `finishReason` (e.g. `RECITATION`) over the
            // unified-enum mapping so a same-protocol round-trip is byte-faithful;
            // refuse when no source terminal was observed.
            "finishReason": rendered_finish_reason(result, PROVIDER_ID_GOOGLE, finish_reason_str)
                .ok_or_else(|| invalid_response("GenerateContent response has no terminal"))?,
            "index": provider_namespace(&result.provider_metadata, PROVIDER_ID_GOOGLE)
                .and_then(|fields| fields.get("candidateIndex")).cloned().unwrap_or(json!(0)),
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
        });
        if let Some(id) = &result.response_id {
            body["responseId"] = id.clone().into();
        }
        if let Some(usage) = usage {
            body["usageMetadata"] = usage;
        }
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
        Box::new(stream::Encoder::default())
    }
}

impl OutboundAdapter for GenerateContentAdapter {
    fn protocol(&self) -> ApiProtocol {
        protocol()
    }

    fn render_request(&self, prompt: &Prompt) -> Result<serde_json::Value> {
        if !prompt.system_provider_metadata.is_empty()
            || prompt.params.parallel_tool_calls.is_some()
        {
            return Err(ModelError::invalid_request(
                "unrepresentable native request metadata or parallel tool constraint",
            ));
        }
        if let Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict,
            ..
        }) = &prompt.response_format
            && (name.is_some() || description.is_some() || strict.is_some())
        {
            return Err(ModelError::invalid_request(
                "response schema metadata has no GenerateContent projection",
            ));
        }
        if prompt.params.store == Some(true) {
            return Err(ModelError::invalid_request(
                "store: true cannot be honored by a Generate Content upstream",
            ));
        }
        let contents = prompt
            .messages
            .iter()
            .map(render_message)
            .collect::<Result<Vec<_>>>()?;
        let mut req = serde_json::Map::new();
        req.insert("contents".into(), contents.into());
        if let Some(system) = &prompt.system {
            req.insert(
                "systemInstruction".into(),
                serde_json::json!({ "parts": [{ "text": system }] }),
            );
        }
        if !prompt.tools.is_empty() {
            req.insert(
                "tools".into(),
                render_generate_content_tools(&prompt.tools)?,
            );
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
        // Metadata without an equivalent was refused above; preserve the
        // remaining schema constraint in its native nesting.
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
        let mut thinking_config =
            extra_value_for_protocol(&prompt.params, &protocol(), "thinkingConfig")
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
        if let Some(effort) = &prompt.params.reasoning_effort {
            if thinking_config.contains_key("thinkingBudget") {
                return Err(ModelError::invalid_request(
                    "Gemini qualitative reasoning effort cannot be combined with thinkingBudget",
                ));
            }
            thinking_config.insert("thinkingLevel".into(), effort.as_str().into());
        }
        if !thinking_config.is_empty() {
            gen_config.insert(
                "thinkingConfig".into(),
                serde_json::Value::Object(thinking_config),
            );
        }
        // Splat remaining Google generation-config extras (responseLogprobs,
        // candidateCount, …) back into the outbound config. Typed fields above
        // win over a same-named extra; the sentinel key carries top-level fields
        // and is skipped here. Only fields Gemini's `GenerationConfig` declares
        // are forwarded. Protocol provenance keeps every other wire's
        // leftovers out; the allow-list still protects native requests from a
        // provider extension accidentally being emitted at the wrong nesting
        // level. Extend it when Google adds a generation-config field.
        for (k, v) in extras_for_protocol(&prompt.params, &protocol()) {
            if k == GOOGLE_TOP_LEVEL_EXTRA_KEY || k == "cachedContent" {
                continue;
            }
            if !GEMINI_GENERATION_CONFIG_KEYS.contains(&k.as_str()) {
                return Err(ModelError::invalid_request(
                    "unsupported GenerateContent config extension",
                ));
            }
            gen_config.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if !gen_config.is_empty() {
            req.insert("generationConfig".into(), gen_config.into());
        }
        // Lift namespaced top-level extras (safetySettings / cachedContent / …)
        // back to the request root.
        if let Some(value) = prompt.params.supplemental_extra.get("cachedContent") {
            req.insert("cachedContent".into(), value.clone());
        }
        for extras in [
            prompt
                .params
                .supplemental_extra
                .get(GOOGLE_TOP_LEVEL_EXTRA_KEY),
            (prompt.params.extra_protocol.as_ref() == Some(&protocol()))
                .then(|| prompt.params.extra.get(GOOGLE_TOP_LEVEL_EXTRA_KEY))
                .flatten(),
        ] {
            if let Some(Value::Object(top)) = extras {
                for (key, value) in top {
                    if matches!(
                        key.as_str(),
                        "model"
                            | "stream"
                            | "contents"
                            | "systemInstruction"
                            | "tools"
                            | "generationConfig"
                    ) {
                        return Err(ModelError::invalid_request(
                            "native extension cannot replace typed request fields",
                        ));
                    }
                    req.insert(key.clone(), value.clone());
                }
            } else if extras.is_some() {
                return Err(ModelError::invalid_request("invalid native root defaults"));
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

    fn parse_response(&self, body: Value) -> Result<GenerateResult> {
        bitrouter_ai::wire::json(&body, 16 * 1024 * 1024)
            .map_err(|_| invalid_response("GenerateContent response exceeds codec bound"))?;
        let failure = |message| invalid_with_usage(&body, message);
        if !body.is_object()
            || body.get("error").is_some_and(|value| !value.is_null())
            || body
                .get("promptFeedback")
                .and_then(|value| value.get("blockReason"))
                .is_some_and(|value| !value.is_null())
        {
            return Err(failure("GenerateContent response reports failure"));
        }
        if body
            .get("responseId")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        {
            return Err(invalid_response(
                "invalid GenerateContent response identity",
            ));
        }
        if body
            .get("promptFeedback")
            .is_some_and(|value| !value.is_object())
        {
            return Err(failure("invalid GenerateContent prompt feedback"));
        }
        let candidates = body
            .get("candidates")
            .and_then(Value::as_array)
            .filter(|candidates| !candidates.is_empty())
            .ok_or_else(|| failure("GenerateContent response missing candidates"))?;
        let mut indices = std::collections::HashSet::new();
        for candidate in candidates {
            if !candidate.is_object()
                || candidate.get("error").is_some_and(|error| !error.is_null())
            {
                return Err(failure("invalid GenerateContent candidate"));
            }
            if candidate
                .get("finishReason")
                .and_then(Value::as_str)
                .and_then(finish_reason)
                .is_none()
            {
                return Err(failure("GenerateContent response has no concrete terminal"));
            }
            if candidate.get("content").is_some_and(|content| {
                !content.is_object()
                    || content
                        .get("parts")
                        .and_then(Value::as_array)
                        .is_none_or(|parts| parts.iter().any(|part| !part.is_object()))
                    || content
                        .get("role")
                        .is_some_and(|role| role.as_str() != Some("model"))
            }) {
                return Err(failure("GenerateContent candidate has invalid content"));
            }
            if let Some(index) = candidate.get("index") {
                let index = index
                    .as_u64()
                    .filter(|index| *index <= i32::MAX as u64)
                    .ok_or_else(|| failure("invalid GenerateContent candidate index"))?;
                if !indices.insert(index) {
                    return Err(failure("duplicate GenerateContent candidate index"));
                }
            }
        }
        // The restricted canonical result selects one candidate; exact native
        // response custody retains the original full candidates array.
        let candidate = candidates[0]
            .as_object()
            .ok_or_else(|| failure("invalid GenerateContent candidate"))?;
        let candidate = Value::Object(candidate.clone());
        let mut parts = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|p| parse_parts(p))
            .transpose()
            .map_err(|_| failure("unsupported GenerateContent response part"))?
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
        if parts
            .iter()
            .any(|part| matches!(part, Content::ToolCall { .. }))
            && !matches!(finish, Some(FinishReason::Stop | FinishReason::ToolCalls))
        {
            // Canonical consumers can execute ToolCall immediately. Do not
            // expose a truncated/failed turn and defer its refusal to rendering.
            return Err(failure(
                "GenerateContent function call turn did not complete",
            ));
        }
        let usage = body.get("usageMetadata").and_then(parse_usage);
        // Generate Content: top-level `responseId`.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        if body
            .get("responseId")
            .is_some_and(|value| value.as_str().is_none_or(str::is_empty))
        {
            return Err(invalid_response(
                "invalid GenerateContent response identity",
            ));
        }
        let response_id = body
            .get("responseId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Gemini's `modelVersion` (the exact model build that served the
        // response) has no dedicated canonical field — carry it at result level
        // under the `google` namespace.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        let mut provider_metadata = ProviderMetadata::new();
        if let Some(index) = candidate.get("index") {
            if index.as_u64().is_none_or(|index| index > i32::MAX as u64) {
                return Err(failure("invalid GenerateContent candidate index"));
            }
            set_provider_metadata(
                &mut provider_metadata,
                PROVIDER_ID_GOOGLE,
                "candidateIndex",
                index.clone(),
            );
        }
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
        Box::new(stream::Decoder::default())
    }

    fn supports_response_format(&self) -> bool {
        true
    }
}

fn render_part(c: &Content) -> Result<Option<Value>> {
    let metadata = match c {
        Content::Text {
            provider_metadata, ..
        }
        | Content::Reasoning {
            provider_metadata, ..
        }
        | Content::File {
            provider_metadata, ..
        }
        | Content::ToolCall {
            provider_metadata, ..
        }
        | Content::ToolResult {
            provider_metadata, ..
        } => Some(provider_metadata),
        _ => None,
    };
    if metadata.is_some_and(|metadata| {
        metadata
            .iter()
            .any(|(provider, fields)| provider != PROVIDER_ID_GOOGLE && !fields.is_null())
    }) {
        return Err(invalid_response(
            "foreign native metadata has no GenerateContent projection",
        ));
    }
    if matches!(
        c,
        Content::Reasoning {
            native: Some(_),
            ..
        }
    ) {
        return Err(invalid_response(
            "foreign native reasoning has no GenerateContent projection",
        ));
    }
    Ok(match c {
        Content::Text {
            text,
            provider_metadata,
        } => {
            let mut part = json!({ "text": text });
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        Content::Reasoning {
            text,
            provider_metadata,
            ..
        } => {
            let mut part = serde_json::json!({ "text": text, "thought": true });
            // Restore the thinking part's `thoughtSignature` continuity token.
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        Content::ToolCall {
            id,
            name,
            arguments,
            provider_metadata,
            provider_executed,
            dynamic,
        } => {
            if *provider_executed || *dynamic {
                return Err(invalid_response(
                    "provider-executed tools have no client functionCall projection",
                ));
            }
            if foreign_tool_identity(provider_metadata) {
                return Err(invalid_response(
                    "ordinary native tool identity cannot be represented in GenerateContent",
                ));
            }
            let args = function_arguments(arguments).ok_or_else(|| {
                invalid_response("GenerateContent tool arguments are not a complete object")
            })?;
            let mut call = json!({ "name": name, "args": args });
            native_id(&mut call, id, provider_metadata, parts::NATIVE_ID)?;
            let mut part = json!({ "functionCall": call });
            // A `functionCall` continuing a reasoning chain carries a
            // `thoughtSignature`; restore it when it round-tripped.
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        // A native function response has an explicit name and object output.
        // Unsupported media/error/denial status cannot be collapsed to text.
        // https://ai.google.dev/api/caching#FunctionResponse
        Content::ToolResult {
            call_id,
            tool_name,
            output,
            provider_metadata,
            dynamic,
        } => {
            if *dynamic {
                return Err(invalid_response(
                    "dynamic tool result has no GenerateContent projection",
                ));
            }
            let name = tool_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| invalid_response("GenerateContent result function name missing"))?;
            let response = match output {
                ToolResultOutput::Json { value } if value.is_object() => value.clone(),
                ToolResultOutput::Text { value } => json!({"result":value}),
                _ => {
                    return Err(invalid_response(
                        "tool output has no lossless GenerateContent projection",
                    ));
                }
            };
            let mut fr = serde_json::json!({ "name": name, "response": response });
            // Native presence is independent from the canonical correlation ID.
            native_id(&mut fr, call_id, provider_metadata, parts::NATIVE_RESULT_ID)?;
            let mut part = json!({ "functionResponse": fr });
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        // Inline bytes -> `inlineData`; a URL -> `fileData`. Gemini keys media by
        // `mimeType`. <https://ai.google.dev/gemini-api/docs/image-understanding>
        Content::File {
            media_type,
            data,
            provider_metadata,
            ..
        } => {
            let mut part = match data {
                DataContent::Base64 { data } => {
                    json!({"inlineData":{"mimeType":media_type,"data":data}})
                }
                DataContent::Url { url } => {
                    json!({"fileData":{"mimeType":media_type,"fileUri":url}})
                }
            };
            apply_thought_signature(&mut part, provider_metadata);
            Some(part)
        }
        Content::Source {
            source: Source::Url { .. },
            ..
        } => None,
        Content::Source {
            source: Source::Document { .. },
            ..
        }
        | Content::ToolApprovalRequest { .. }
        | Content::ToolApprovalResponse { .. } => {
            return Err(invalid_response(
                "content has no GenerateContent projection",
            ));
        }
    })
}

fn foreign_tool_identity(metadata: &ProviderMetadata) -> bool {
    provider_namespace(metadata, "openai").is_some_and(|fields| {
        fields.contains_key("namespace")
            || fields.get("type").and_then(Value::as_str) == Some("custom_tool_call")
    })
}

fn function_arguments(arguments: &str) -> Option<Value> {
    if arguments.len() > parts::MAX_BYTES {
        return None;
    }
    native::value(arguments).ok().filter(Value::is_object)
}

fn render_parts(parts: &[Content]) -> Result<Vec<Value>> {
    parts
        .iter()
        .filter_map(|part| render_part(part).transpose())
        .collect()
}

fn render_message(message: &Message) -> Result<Value> {
    Ok(json!({ "role": role_str(message.role), "parts": render_parts(&message.content)? }))
}

/// Render the official Gemini `UsageMetadata` shape. Gemini exposes cache
/// reads as a subset of `promptTokenCount`, but has no cache-write counter.
fn render_usage(usage: &Usage) -> Result<Value> {
    let limit = i32::MAX as u64;
    let total = usage
        .prompt_tokens
        .checked_add(usage.completion_tokens)
        .filter(|total| *total <= limit)
        .ok_or_else(|| invalid_response("GenerateContent usage exceeds exact bound"))?;
    if usage.reasoning_tokens > usage.completion_tokens
        || usage.cache_read_tokens > usage.prompt_tokens
    {
        return Err(invalid_response(
            "GenerateContent usage counters contradict",
        ));
    }
    let mut value = json!({ "promptTokenCount": usage.prompt_tokens,
        "candidatesTokenCount": usage.completion_tokens - usage.reasoning_tokens,
        "totalTokenCount": total, "thoughtsTokenCount": usage.reasoning_tokens });
    if usage.cache_read_tokens > 0 {
        value["cachedContentTokenCount"] = usage.cache_read_tokens.into();
    }
    Ok(value)
}

fn invalid_with_usage(body: &Value, message: &'static str) -> ModelError {
    ModelError::InvalidResponse {
        message: message.into(),
        usage: body
            .get("usageMetadata")
            .and_then(parse_usage)
            .map(Box::new),
    }
}

fn native_id(
    target: &mut Value,
    canonical: &str,
    metadata: &ProviderMetadata,
    key: &str,
) -> Result<()> {
    match provider_namespace(metadata, PROVIDER_ID_GOOGLE).and_then(|fields| fields.get(key)) {
        Some(Value::Null) => (),
        Some(Value::String(id)) if !id.is_empty() => target["id"] = id.clone().into(),
        Some(_) => return Err(invalid_response("invalid retained native call identity")),
        None if !canonical.is_empty() => target["id"] = canonical.into(),
        None => return Err(invalid_response("tool identity absent")),
    }
    Ok(())
}

fn parse_usage(value: &serde_json::Value) -> Option<Usage> {
    // Absence of `promptTokenCount` means the chunk carries no usage at all.
    let limit = i32::MAX as u64;
    let prompt = value
        .get("promptTokenCount")?
        .as_u64()
        .filter(|value| *value <= limit)?;
    let count = |key| value.get(key).map_or(Some(0), Value::as_u64);
    let candidates = count("candidatesTokenCount").filter(|value| *value <= limit)?;
    let reasoning = count("thoughtsTokenCount").filter(|value| *value <= limit)?;
    // Optional scalar counts default only when absent.
    let cache_read = count("cachedContentTokenCount").filter(|value| *value <= prompt)?;
    for (field, expected) in [
        ("promptTokensDetails", prompt),
        ("cacheTokensDetails", cache_read),
        ("candidatesTokensDetails", candidates),
        ("toolUsePromptTokensDetails", 0),
    ] {
        if let Some(details) = value.get(field) {
            let mut sum = 0_u64;
            for detail in details.as_array()? {
                let object = detail.as_object()?;
                if object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "modality" | "tokenCount"))
                    || object.get("modality")?.as_str()? != "TEXT"
                {
                    return None;
                }
                let count = object
                    .get("tokenCount")
                    .map_or(Some(0), Value::as_u64)
                    .filter(|value| *value <= limit)?;
                sum = sum.checked_add(count)?;
            }
            if sum != expected {
                return None;
            }
        }
    }
    let total = prompt
        .checked_add(candidates)?
        .checked_add(reasoning)
        .filter(|total| *total <= limit)?;
    // Native scalar counts are implicit-presence int32. Keep the original raw
    // object, including omitted fields, separately from the normalized counters.
    // https://github.com/googleapis/googleapis/blob/master/google/ai/generativelanguage/v1beta/generative_service.proto
    let tool_prompt = count("toolUsePromptTokenCount").filter(|value| *value <= limit)?;
    if tool_prompt != 0 {
        return None;
    } // No canonical billing slot for this independent usage.
    if let Some(reported) = value.get("totalTokenCount")
        && reported.as_u64()? != total
    {
        return None;
    }
    Some(Usage {
        prompt_tokens: prompt,
        // Gemini reports visible candidate tokens and billed thought tokens as
        // sibling counters. Canonical `completion_tokens` is inclusive so
        // reasoning remains a subset, matching the other protocol adapters.
        completion_tokens: candidates.checked_add(reasoning)?,
        reasoning_tokens: reasoning,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        web_search_count: 0,
        origin: UsageOrigin::ProviderReported,
        raw: Some(Box::new(value.clone())),
    })
}

#[cfg(test)]
mod tests;
