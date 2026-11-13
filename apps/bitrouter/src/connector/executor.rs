//! One fixed member, one durable send attempt, and conservative terminal evidence.

use std::time::Duration;

use axum::body::Bytes;
use bitrouter_ai::types::{ApiProtocol, StreamPart};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::gateway::model::{
    client::{HttpTimeouts, ModelClient},
    protocol::{
        OutboundAdapter, OutboundDispatch, SseEvent, chat_completions::ChatCompletionsAdapter,
    },
};

use super::{
    config::{ConnectorConfig, MemberConfig},
    controller::Controller,
    journal::{ExecutionRecord, TerminalEvidence},
    request::LocalRequest,
};

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Executor {
    client: reqwest::Client,
    model: ModelClient,
}

impl Executor {
    pub fn new(config: &ConnectorConfig) -> Result<Self, &'static str> {
        let execution = config
            .execution
            .as_ref()
            .ok_or("execution_not_configured")?;
        let timeout = Duration::from_millis(execution.max_deadline_ms);
        let idle = Duration::from_millis(execution.stream_idle_timeout_ms);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(idle.min(timeout))
            .read_timeout(idle)
            .timeout(timeout)
            .build()
            .map_err(|_| "http_client")?;
        let model = ModelClient::with_http_client(
            HttpTimeouts {
                total: Some(timeout),
                ..Default::default()
            },
            std::sync::Arc::new(OutboundDispatch::builtin()),
            client.clone(),
        );
        Ok(Self { client, model })
    }

    /// vLLM renders the same fixed text-chat template used by the generation call.
    /// This endpoint performs tokenization, never a generation or fallback.
    pub async fn count_input(
        &self,
        config: &ConnectorConfig,
        member: &MemberConfig,
        request: &LocalRequest,
        deadline: Instant,
    ) -> Result<u32, &'static str> {
        let execution = config
            .execution
            .as_ref()
            .ok_or("execution_not_configured")?;
        let mut url = Url::parse(&member.inference_endpoint).map_err(|_| "target_configuration")?;
        url.set_path("/tokenize");
        let mut wire = self
            .client
            .post(url)
            .json(&request.tokenize_body())
            .build()
            .map_err(|_| "tokenization_request")?;
        authenticate(&mut wire, member)?;
        let response = tokio::time::timeout_at(deadline, self.client.execute(wire))
            .await
            .map_err(|_| "deadline")?
            .map_err(|_| "tokenization_transport")?;
        if !response.status().is_success() {
            return Err("tokenization_http_status");
        }
        let bytes = tokio::time::timeout_at(
            deadline,
            bounded_body(response, execution.max_response_bytes),
        )
        .await
        .map_err(|_| "deadline")??;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "tokenization_response")?;
        let count = value
            .get("count")
            .and_then(Value::as_u64)
            .ok_or("tokenization_response")?;
        let tokens = value
            .get("tokens")
            .and_then(Value::as_array)
            .ok_or("tokenization_response")?;
        let context = value
            .get("max_model_len")
            .and_then(Value::as_u64)
            .ok_or("tokenization_response")?;
        if count == 0
            || count != tokens.len() as u64
            || tokens.iter().any(|token| token.as_u64().is_none())
        {
            return Err("tokenization_response");
        }
        if count > u64::from(execution.max_input_tokens) {
            return Err("input_token_limit");
        }
        if count.saturating_add(u64::from(request.request.max_tokens)) > context {
            return Err("model_context_limit");
        }
        u32::try_from(count).map_err(|_| "input_token_limit")
    }

    /// Build and authenticate before durable intent; no network I/O occurs here.
    pub fn build(
        &self,
        member: &MemberConfig,
        request: &LocalRequest,
    ) -> Result<reqwest::Request, &'static str> {
        let url = format!(
            "{}/chat/completions",
            member.inference_endpoint.trim_end_matches('/')
        );
        let mut wire = self
            .model
            .build_request(&url, &request.inference_body())
            .map_err(|_| "inference_request")?;
        authenticate(&mut wire, member)?;
        Ok(wire)
    }

    pub async fn execute(
        &self,
        controller: Controller,
        reserved: ExecutionRecord,
        request: LocalRequest,
        wire: reqwest::Request,
        deadline: Instant,
        output: mpsc::Sender<Bytes>,
    ) {
        // Receiver closure means delivery ended, never that the engine stopped.
        let operation = async {
            let sending = controller
                .begin_send(&reserved)
                .await
                .map_err(|error| error.code())?;
            let cancellation = CancellationToken::new();
            let result = async {
                let response = self
                    .model
                    .send(wire, &cancellation)
                    .await
                    .map_err(|_| "upstream_transport")?;
                controller
                    .mark_running(&sending)
                    .await
                    .map_err(|error| error.code())?;
                if !response.status().is_success() {
                    return Err("upstream_http_status");
                }
                let config = controller.config();
                let execution = config
                    .execution
                    .as_ref()
                    .ok_or("execution_not_configured")?;
                if request.request.stream {
                    self.stream(
                        response,
                        &request,
                        sending.input_tokens,
                        execution.max_response_bytes,
                        &output,
                    )
                    .await?;
                    controller
                        .complete(&sending, TerminalEvidence::Succeeded)
                        .await
                        .map_err(|error| error.code())?;
                    let _ = output.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
                } else {
                    let body = bounded_body(response, execution.max_response_bytes).await?;
                    validate_completion(&body, &request, sending.input_tokens)?;
                    controller
                        .complete(&sending, TerminalEvidence::Succeeded)
                        .await
                        .map_err(|error| error.code())?;
                    let _ = output.send(Bytes::from(body)).await;
                }
                Ok::<(), &'static str>(())
            }
            .await;
            if let Err(reason) = result {
                let _ = controller.mark_unknown(&sending, reason).await;
                return Err(reason);
            }
            Ok(())
        };
        let result = tokio::select! {
            biased;
            _ = output.closed() => Err("client_disconnected"),
            _ = tokio::time::sleep_until(deadline) => Err("deadline"),
            result = operation => result,
        };
        if let Err(reason) = result {
            // cancel_before_send releases only a still-unsent reservation. A
            // durable sending/running record is kept unknown on all ambiguity.
            if controller
                .cancel_before_send(&reserved.request_id)
                .await
                .is_err()
            {
                let _ = controller.mark_unknown(&reserved, reason).await;
            }
            let body = if request.request.stream {
                format!("event: error\ndata: {{\"error\":{{\"code\":\"{reason}\"}}}}\n\n")
            } else {
                format!(
                    "{{\"error\":{{\"code\":\"{reason}\"}},\"request_id\":\"{}\"}}",
                    request.request_id
                )
            };
            let _ = output.send(Bytes::from(body)).await;
        }
    }

    async fn stream(
        &self,
        response: reqwest::Response,
        request: &LocalRequest,
        input_tokens: u32,
        limit: usize,
        output: &mpsc::Sender<Bytes>,
    ) -> Result<(), &'static str> {
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|header| header.to_str().ok())
            .is_some_and(|value| value.split(';').next() == Some("text/event-stream"))
        {
            return Err("invalid_stream_content_type");
        }
        let bytes = async_stream::try_stream! {
            let mut response = response;
            let mut received = 0usize;
            let mut unfinished = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| "upstream_transport")? {
                if chunk.len() > limit.saturating_sub(received) {
                    Err("response_body_limit")?;
                }
                received += chunk.len();
                unfinished.extend_from_slice(&chunk);
                let mut consumed = 0usize;
                while let Some(boundary) = frame_boundary(&unfinished[consumed..]) {
                    validate_frame(&unfinished[consumed..consumed + boundary])?;
                    consumed += boundary;
                }
                unfinished.drain(..consumed);
                yield chunk;
            }
            // eventsource-stream intentionally drops an unterminated final
            // event. It cannot turn a truncated trailing frame into clean EOF.
            if unfinished.iter().any(|byte| !byte.is_ascii_whitespace()) {
                Err("incomplete_stream_frame")?;
            }
        };
        let mut events = Box::pin(bytes).eventsource();
        let mut decoder = ChatCompletionsAdapter.stream_decoder();
        let mut terminal = false;
        let mut done = false;
        let mut usage = false;
        let mut response_id: Option<String> = None;
        while let Some(event) = events.next().await {
            let event = event.map_err(|_: eventsource_stream::EventStreamError<&'static str>| "invalid_or_interrupted_stream")?;
            if done || (!event.event.is_empty() && event.event != "message") {
                return Err("invalid_stream_event");
            }
            if event.data == "[DONE]" {
                if !terminal || !usage {
                    return Err("missing_terminal_evidence");
                }
                done = true;
            } else {
                let value: Value =
                    serde_json::from_str(&event.data).map_err(|_| "invalid_stream_json")?;
                if value.get("error").is_some_and(|value| !value.is_null()) {
                    return Err("upstream_stream_error");
                }
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("invalid_response_identity")?;
                if response_id
                    .as_deref()
                    .is_some_and(|previous| previous != id)
                    || value.get("model").and_then(Value::as_str)
                        != Some(request.request.model.as_str())
                {
                    return Err("invalid_response_identity");
                }
                response_id = Some(id.to_owned());
                let choices = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .ok_or("invalid_stream_choices")?;
                match choices.as_slice() {
                    [] if terminal => {}
                    [choice] if !terminal => {
                        if choice.get("index").and_then(Value::as_u64) != Some(0)
                            || choice.get("delta").and_then(Value::as_object).is_none()
                            || choice.get("delta").is_some_and(has_tool_work)
                            || choice
                                .get("delta")
                                .is_some_and(|delta| !valid_text_fields(delta))
                        {
                            return Err("unsupported_stream_choice");
                        }
                        if let Some(reason) =
                            choice.get("finish_reason").filter(|value| !value.is_null())
                        {
                            if !valid_finish(reason) {
                                return Err("invalid_finish_reason");
                            }
                            terminal = true;
                        }
                    }
                    _ => return Err("invalid_stream_order"),
                }
                if let Some(reported) = value.get("usage").filter(|value| !value.is_null()) {
                    if !terminal || usage {
                        return Err("invalid_usage_order");
                    }
                    validate_usage(reported, input_tokens, request.request.max_tokens)?;
                    usage = true;
                }
            }
            let parts = decoder
                .decode(&SseEvent {
                    event: None,
                    data: event.data.clone(),
                })
                .map_err(|_| "invalid_stream_protocol")?;
            if parts
                .iter()
                .any(|part| matches!(part, StreamPart::ToolCallDelta { .. }))
            {
                return Err("unsupported_stream_choice");
            }
            if !done {
                output
                    .send(Bytes::from(format!("data: {}\n\n", event.data)))
                    .await
                    .map_err(|_| "client_disconnected")?;
            }
        }
        decoder.finish().map_err(|_| "invalid_stream_protocol")?;
        if !done || !terminal || !usage {
            return Err("missing_terminal_evidence");
        }
        Ok(())
    }
}

fn authenticate(request: &mut reqwest::Request, member: &MemberConfig) -> Result<(), &'static str> {
    if let Some(name) = &member.inference_bearer_env {
        let value = std::env::var(name).map_err(|_| "inference_authentication_unavailable")?;
        if value.is_empty() {
            return Err("inference_authentication_unavailable");
        }
        let mut header = HeaderValue::from_str(&format!("Bearer {value}"))
            .map_err(|_| "inference_authentication_unavailable")?;
        header.set_sensitive(true);
        request.headers_mut().insert(AUTHORIZATION, header);
    }
    Ok(())
}

pub async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err("response_body_limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "upstream_transport")? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err("response_body_limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn valid_finish(value: &Value) -> bool {
    matches!(value.as_str(), Some("stop" | "length" | "content_filter"))
}

fn has_tool_work(message: &Value) -> bool {
    let calls = message
        .get("tool_calls")
        .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty));
    calls
        || message
            .get("function_call")
            .is_some_and(|value| !value.is_null())
}

fn valid_text_fields(message: &Value) -> bool {
    [
        "content",
        "reasoning_content",
        "reasoning",
        "thinking",
        "refusal",
    ]
    .iter()
    .all(|field| {
        message
            .get(*field)
            .is_none_or(|value| value.is_null() || value.is_string())
    }) && message.get("audio").is_none_or(Value::is_null)
}

fn frame_boundary(bytes: &[u8]) -> Option<usize> {
    bytes.iter().enumerate().find_map(|(index, byte)| {
        if *byte != b'\n' {
            return None;
        }
        if bytes.get(index + 1) == Some(&b'\n') {
            return Some(index + 2);
        }
        if bytes.get(index + 1) == Some(&b'\r') && bytes.get(index + 2) == Some(&b'\n') {
            return Some(index + 3);
        }
        None
    })
}

fn validate_frame(frame: &[u8]) -> Result<(), &'static str> {
    let frame = std::str::from_utf8(frame).map_err(|_| "invalid_stream_frame")?;
    for line in frame.lines() {
        if line.is_empty()
            || line.starts_with(':')
            || line.starts_with("data:")
            || line.starts_with("event:")
        {
            continue;
        }
        // This direct vLLM slice has no resume IDs or retry instructions. SSE
        // parsers normally ignore unknown fields, including malformed trailers;
        // silently accepting them would weaken durable completion evidence.
        return Err("invalid_stream_frame");
    }
    Ok(())
}

fn validate_usage(usage: &Value, input: u32, output: u32) -> Result<(), &'static str> {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .ok_or("missing_usage")?;
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .ok_or("missing_usage")?;
    if prompt != u64::from(input) || completion > u64::from(output) {
        return Err("token_boundary_violation");
    }
    Ok(())
}

fn validate_completion(
    bytes: &[u8],
    request: &LocalRequest,
    input: u32,
) -> Result<(), &'static str> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid_response_json")?;
    if value.get("model").and_then(Value::as_str) != Some(request.request.model.as_str())
        || value
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || value.get("error").is_some_and(|value| !value.is_null())
    {
        return Err("invalid_response_identity");
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or("missing_terminal_evidence")?;
    let [choice] = choices.as_slice() else {
        return Err("invalid_response_choices");
    };
    if choice.get("index").and_then(Value::as_u64) != Some(0)
        || !choice.get("finish_reason").is_some_and(valid_finish)
        || choice
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            != Some("assistant")
        || choice.get("message").is_some_and(has_tool_work)
        || choice
            .get("message")
            .is_some_and(|message| !valid_text_fields(message))
    {
        return Err("missing_terminal_evidence");
    }
    validate_usage(
        value.get("usage").ok_or("missing_usage")?,
        input,
        request.request.max_tokens,
    )?;
    ModelClient::parse_response(
        &ChatCompletionsAdapter,
        &ApiProtocol::ChatCompletions,
        std::str::from_utf8(bytes).map_err(|_| "invalid_response_json")?,
    )
    .map_err(|_| "invalid_response_protocol")?;
    Ok(())
}
