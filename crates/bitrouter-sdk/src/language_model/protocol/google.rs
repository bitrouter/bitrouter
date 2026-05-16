//! Google Generative AI (`generateContent`) adapter.
//!
//! Official reference: <https://ai.google.dev/api/generate-content>
//! Streaming: <https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent>
//!
//! Google uses `contents[]` with roles `user` / `model`, a separate
//! `systemInstruction`, and `parts[]` of `{text}` / `{functionCall}` /
//! `{functionResponse}`. Reasoning is a `part` flagged `thought: true`
//! (v0 #454-1: such parts must not be dropped).

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{BitrouterError, Result};
use crate::language_model::protocol::{
    InboundAdapter, OutboundAdapter, SseEvent, StreamDecoder, StreamEncoder, Transport,
    describe_deser_error,
};
use crate::language_model::stream::SseFrame;
use crate::language_model::types::{
    ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message, Prompt, Role,
    RoutingTarget, StreamPart, Usage,
};

/// The Google Generative AI protocol adapter.
pub struct GoogleAdapter;

/// HTTP transport for Google Generative AI:
/// `POST {api_base}/models/{model}:generateContent` (or
/// `:streamGenerateContent?alt=sse` for streaming) with the `x-goog-api-key`
/// header — documented at
/// <https://ai.google.dev/gemini-api/docs/api-key>.
pub struct GoogleTransport;

/// Sentinel key under which top-level Google extras (`toolConfig`,
/// `safetySettings`, `cachedContent`, …) ride through `GenerationParams::extra`.
/// Only `GoogleAdapter::render_request` reads it — every other adapter ignores
/// the namespaced key and the JSON wire shape never contains it.
const GOOGLE_TOP_LEVEL_EXTRA_KEY: &str = "__google_top_level__";

// ===== wire request types =====

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest {
    /// Carried as a field so the inbound HTTP route's `{model}` path param can
    /// override it; defaults empty.
    #[serde(default)]
    model: String,
    contents: Vec<GoogleContent>,
    #[serde(default)]
    system_instruction: Option<GoogleContent>,
    #[serde(default)]
    tools: Vec<GoogleTool>,
    #[serde(default)]
    generation_config: Option<GoogleGenerationConfig>,
    /// Injected by `server::google_generate` from the path verb
    /// (`:streamGenerateContent` → true, `:generateContent` → false).
    #[serde(default)]
    stream: bool,
    /// Top-level extras: `toolConfig`, `safetySettings`, `cachedContent`, … —
    /// preserve them across the inbound→outbound round-trip. Per
    /// <https://ai.google.dev/api/generate-content>.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GoogleContent {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    parts: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTool {
    #[serde(default)]
    function_declarations: Vec<GoogleFunctionDecl>,
}

#[derive(Debug, Deserialize)]
struct GoogleFunctionDecl {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleGenerationConfig {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    /// `stopSequences`, `seed`, `topK`, `responseMimeType`, `responseSchema`,
    /// `responseLogprobs`, `presencePenalty`, `frequencyPenalty`, … — every
    /// generation-config knob without a typed slot rides via `extra` and is
    /// splatted back into `generationConfig` on render. v0 passed these through.
    #[serde(flatten)]
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
        // Google has only user/model; tool results ride in a user turn.
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

impl InboundAdapter for GoogleAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Google
    }

    fn parse_request(&self, body: serde_json::Value) -> Result<Prompt> {
        let req: GenerateContentRequest = serde_json::from_value(body.clone())
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

        let mut params = req
            .generation_config
            .map(|g| GenerationParams {
                temperature: g.temperature,
                top_p: g.top_p,
                max_tokens: g.max_output_tokens,
                reasoning_effort: None,
                extra: g.extra,
            })
            .unwrap_or_default();
        // Preserve top-level Google fields (`toolConfig`, `safetySettings`,
        // `cachedContent`, …) across the round-trip. They're namespaced so they
        // don't collide with `generationConfig`-level extras above and only the
        // Google `render_request` lifts them back to the top level.
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
        Box::new(GoogleStreamEncoder)
    }
}

impl OutboundAdapter for GoogleAdapter {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Google
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
        // Splat Google generation-config extras (stopSequences, topK, seed,
        // responseMimeType / responseSchema, …) back into the outbound config.
        // Typed fields above win over a same-named extra; the sentinel key
        // carries top-level fields and is skipped here.
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
        Ok(GenerateResult {
            content: parts,
            usage,
            finish_reason: finish,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GoogleStreamDecoder::default())
    }
}

#[async_trait]
impl Transport for GoogleTransport {
    fn protocol(&self) -> ApiProtocol {
        ApiProtocol::Google
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

/// Google `streamGenerateContent` SSE decoder. Each `data:` line is a full
/// `GenerateContentResponse` with partial candidates.
#[derive(Default)]
struct GoogleStreamDecoder {
    finished: bool,
}

impl StreamDecoder for GoogleStreamDecoder {
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

/// Google `streamGenerateContent` SSE encoder — each canonical part becomes one
/// `GenerateContentResponse` chunk.
struct GoogleStreamEncoder;

impl StreamEncoder for GoogleStreamEncoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let chunk = match part {
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
                // Inbound was OpenAI Responses; Google has no response-completed
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
        // Google surfaces a mid-stream error as a chunk carrying an `error`
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
