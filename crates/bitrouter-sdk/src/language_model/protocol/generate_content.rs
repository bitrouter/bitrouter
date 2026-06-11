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
    InboundAdapter, OutboundAdapter, SseEvent, StreamDecoder, StreamEncoder, Transport,
    describe_deser_error,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message, Prompt,
    ResponseFormat, Role, RoutingTarget, StreamPart, ToolChoice, Usage,
};

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

/// One element of [`GenerateContentRequest`]'s `tools` array — Google's
/// `{ functionDeclarations: [...] }` envelope.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentTool {
    #[serde(default)]
    function_declarations: Vec<GenerateContentFunctionDecl>,
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
    /// `stopSequences`, `seed`, `topK`, `responseLogprobs`, `presencePenalty`,
    /// `frequencyPenalty`, … — every generation-config knob without a typed
    /// slot rides via `extra` and is splatted back into `generationConfig` on
    /// render. v0 passed these through. Skipped from the published schema for
    /// the same reason as the top-level `extra` on [`GenerateContentRequest`].
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
            });
        } else if let Some(fr) = part.get("functionResponse") {
            out.push(Content::ToolResult {
                call_id: fr
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                content: fr
                    .get("response")
                    .map(|r| r.to_string())
                    .unwrap_or_default(),
            });
        } else if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            if is_thought {
                out.push(Content::Reasoning {
                    text: text.to_string(),
                });
            } else {
                out.push(Content::Text {
                    text: text.to_string(),
                });
            }
        }
        // parts of other shapes (inlineData, fileData…) are skipped for now
    }
    out
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

        let tools = req
            .tools
            .into_iter()
            .flat_map(|t| t.function_declarations)
            .map(|f| crate::language_model::types::Tool {
                name: f.name,
                description: f.description,
                parameters: f.parameters,
            })
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
                    mut extra,
                } = g;
                let response_format = match (
                    response_mime_type.as_deref() == Some("application/json"),
                    response_schema,
                ) {
                    (true, Some(schema)) => Some(ResponseFormat::JsonSchema {
                        name: None,
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
                (
                    GenerationParams {
                        temperature,
                        top_p,
                        max_tokens: max_output_tokens,
                        reasoning_effort: None,
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
        Ok(serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": parts },
                "finishReason": result
                    .finish_reason
                    .as_ref()
                    .map(finish_reason_str)
                    .unwrap_or_else(|| "STOP".to_string()),
                "index": 0,
            }],
            "usageMetadata": {
                "promptTokenCount": usage.prompt_tokens,
                "candidatesTokenCount": usage.completion_tokens,
                "totalTokenCount": usage.total(),
                "thoughtsTokenCount": usage.reasoning_tokens,
            },
        }))
    }

    fn stream_encoder(&self, _request_id: &str, _model: &str) -> Box<dyn StreamEncoder> {
        Box::new(GenerateContentStreamEncoder)
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
            req.insert(
                "tools".into(),
                serde_json::json!([{
                    "functionDeclarations": prompt.tools.iter().map(|t| serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })).collect::<Vec<_>>()
                }]),
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
        // `name` and `strict` are intentionally dropped — Google's
        // schema-constrained sampling has no concept of either.
        if let Some(rf) = &prompt.response_format {
            let ResponseFormat::JsonSchema { schema, .. } = rf;
            gen_config.insert("responseMimeType".into(), "application/json".into());
            gen_config.insert("responseSchema".into(), schema.clone());
        }
        // Splat Google generation-config extras (stopSequences, topK, seed, …)
        // back into the outbound config. Typed fields above win over a same-named
        // extra; the sentinel key carries top-level fields and is skipped here.
        for (k, v) in &prompt.params.extra {
            if k == GOOGLE_TOP_LEVEL_EXTRA_KEY {
                continue;
            }
            gen_config.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if !gen_config.is_empty() {
            req.insert("generationConfig".into(), gen_config.into());
        }
        // Lift namespaced top-level extras (toolConfig / safetySettings /
        // cachedContent / …) back to the request root.
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
        let parts = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|p| parse_parts(p))
            .unwrap_or_default();
        let finish = candidate
            .get("finishReason")
            .and_then(|f| f.as_str())
            .and_then(finish_reason);
        let usage = body.get("usageMetadata").and_then(parse_usage);
        // Generate Content: top-level `responseId`.
        // <https://ai.google.dev/api/generate-content#GenerateContentResponse>
        let response_id = body
            .get("responseId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(GenerateResult {
            content: parts,
            usage,
            finish_reason: finish,
            response_id,
            stop_details: None,
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
        Content::Text { text } => Some(serde_json::json!({ "text": text })),
        Content::Reasoning { text } => Some(serde_json::json!({ "text": text, "thought": true })),
        Content::ToolCall {
            name, arguments, ..
        } => {
            let args: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
            Some(serde_json::json!({ "functionCall": { "name": name, "args": args } }))
        }
        Content::ToolResult { call_id, content } => {
            let response: serde_json::Value =
                serde_json::from_str(content).unwrap_or(serde_json::json!({ "result": content }));
            Some(serde_json::json!({
                "functionResponse": { "name": call_id, "response": response }
            }))
        }
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
                        Content::Text { text } => parts.push(StreamPart::TextDelta { text }),
                        Content::Reasoning { text } => {
                            parts.push(StreamPart::ReasoningDelta { text })
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => parts.push(StreamPart::ToolCallDelta {
                            id,
                            name: Some(name),
                            arguments,
                        }),
                        Content::ToolResult { .. } => {}
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

/// Generate Content `streamGenerateContent` SSE encoder — each canonical part becomes one
/// `GenerateContentResponse` chunk.
struct GenerateContentStreamEncoder;

impl StreamEncoder for GenerateContentStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let chunk = match part {
            // Observability-only metadata (upstream response id) — never
            // forwarded to the Google-protocol client.
            StreamPart::ResponseStarted { .. } => return Ok(Vec::new()),
            StreamPart::TextDelta { text } => serde_json::json!({
                "candidates": [{ "content": { "role": "model", "parts": [{ "text": text }] } }]
            }),
            StreamPart::ReasoningDelta { text } => serde_json::json!({
                "candidates": [{ "content": { "role": "model",
                    "parts": [{ "text": text, "thought": true }] } }]
            }),
            StreamPart::ToolCallDelta {
                name, arguments, ..
            } => {
                let args: serde_json::Value = serde_json::from_str(if arguments.is_empty() {
                    "{}"
                } else {
                    arguments
                })
                .unwrap_or(serde_json::json!({}));
                serde_json::json!({
                    "candidates": [{ "content": { "role": "model", "parts": [{
                        "functionCall": { "name": name.clone().unwrap_or_default(), "args": args }
                    }] } }]
                })
            }
            StreamPart::Usage { usage } => serde_json::json!({
                "usageMetadata": {
                    "promptTokenCount": usage.prompt_tokens,
                    "candidatesTokenCount": usage.completion_tokens,
                    "totalTokenCount": usage.total(),
                    "thoughtsTokenCount": usage.reasoning_tokens,
                }
            }),
            StreamPart::Finish { reason } => serde_json::json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [] },
                    "finishReason": finish_reason_str(reason),
                }]
            }),
            StreamPart::ResponseCompleted { status, usage, .. } => {
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
                chunk
            }
        };
        Ok(vec![SseFrame::Event {
            event: None,
            data: chunk.to_string(),
        }])
    }

    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        // Generate Content surfaces a mid-stream error as a chunk carrying an `error`
        // object (mirrors the non-streaming error envelope).
        vec![SseFrame::Event {
            event: None,
            data: serde_json::json!({
                "error": { "code": 502, "status": "UNAVAILABLE", "message": message }
            })
            .to_string(),
        }]
    }
}
