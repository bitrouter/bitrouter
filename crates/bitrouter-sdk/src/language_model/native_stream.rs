//! Streaming display for a durably admitted attempt. The accumulated result is
//! still subject to the ordinary output, accounting and acknowledgement gates.

use std::collections::BTreeMap;

use futures::StreamExt;

use crate::error::{BitrouterError, Result};

use super::executor::StreamPartStream;
use super::native::NativeExecutionControl;
use super::stream::{StreamOutcome, StreamProcessor};
use bitrouter_ai::types::{Content, FinishReason, GenerateResult, ReasoningTextKind, StreamPart};

fn invalid(message: &str) -> BitrouterError {
    BitrouterError::UpstreamInvalidResponse {
        message: message.into(),
        usage: None,
    }
}

/// Errors after stream admission are returned alongside partial usage, never
/// as a retryable executor error. Partial content cannot authorize tool calls.
pub(super) async fn collect(
    mut upstream: StreamPartStream,
    mut processor: StreamProcessor,
    control: &dyn NativeExecutionControl,
    request_id: &str,
) -> (GenerateResult, Option<BitrouterError>) {
    let mut collector = Collector::new(
        control
            .provider_response_byte_limit()
            .unwrap_or(4 * 1024 * 1024),
    );
    let error = 'parts: loop {
        let next = tokio::select! {
            biased;
            _ = control.provider_cancelled() => {
                break Some(invalid("managed provider stream cancelled"));
            }
            next = upstream.next() => next,
        };
        let Some(next) = next else {
            break collector
                .result
                .finish_reason
                .is_none()
                .then(|| invalid("provider stream ended without a terminal event"));
        };
        let part = match next {
            Ok(part) => part,
            Err(_) => break Some(invalid("provider stream interrupted")),
        };
        // Bound custom executor frames before policy callbacks can clone them.
        if !super::native_output::fits(&part, collector.remaining) {
            break Some(invalid("provider stream exceeded its byte allowance"));
        }
        let processed = tokio::select! {
            biased;
            _ = control.provider_cancelled() => {
                break Some(invalid("managed provider stream cancelled"));
            }
            parts = processor.process_part(part) => parts,
        };
        let parts = match processed {
            Ok(parts) => parts,
            Err(_) => break Some(invalid("provider stream rejected by policy")),
        };
        for part in parts {
            if let Err(error) = collector.observe(&part) {
                break 'parts Some(error);
            }
            tokio::select! {
                biased;
                _ = control.provider_cancelled() => {
                    break 'parts Some(invalid("managed provider stream cancelled"));
                }
                () = control.on_stream_part(request_id, &part) => {}
            }
        }
    };
    drop(upstream);
    let outcome = error.as_ref().map_or(StreamOutcome::Completed, |error| {
        StreamOutcome::UpstreamError(error.clone())
    });
    // Billing uses original upstream counters, even when stream policy rewrites
    // usage or aborts. Display projections never become accounting evidence.
    collector.result.usage = processor.finish(outcome).await.final_usage.clone();
    if error.is_some() {
        collector.result.finish_reason = Some(FinishReason::Error(
            "managed provider stream did not complete".into(),
        ));
    }
    (collector.result, error)
}

struct Collector {
    result: GenerateResult,
    tools: BTreeMap<String, usize>,
    reasoning: BTreeMap<String, usize>,
    active_reasoning: Option<usize>,
    reasoning_summaries: BTreeMap<usize, String>,
    remaining: u64,
    terminal: bool,
}

impl Collector {
    fn new(limit: u64) -> Self {
        Self {
            result: super::native_output::empty_result(),
            tools: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            active_reasoning: None,
            reasoning_summaries: BTreeMap::new(),
            remaining: limit,
            terminal: false,
        }
    }

    fn observe(&mut self, part: &StreamPart) -> Result<()> {
        if !super::native_output::fits(part, self.remaining) {
            return Err(invalid(
                "collected provider stream exceeded its byte allowance",
            ));
        }
        let bytes = serde_json::to_vec(part)
            .map_err(|_| invalid("provider stream part could not be encoded"))?;
        self.remaining = self.remaining.saturating_sub(bytes.len() as u64);
        // Some wires report usage after Finish. Content after termination is
        // invalid, but the trailing usage still reaches the billing processor.
        if self.terminal
            && !matches!(
                part,
                StreamPart::Usage { .. } | StreamPart::ResponseCompleted { .. }
            )
        {
            return Err(invalid("provider stream emitted content after termination"));
        }
        match part {
            StreamPart::TextStart { .. } => self.result.content.push(Content::Text {
                text: String::new(),
                provider_metadata: Default::default(),
            }),
            StreamPart::TextDelta { text } => match self.result.content.last_mut() {
                Some(Content::Text { text: current, .. }) => current.push_str(text),
                _ => self.result.content.push(Content::Text {
                    text: text.clone(),
                    provider_metadata: Default::default(),
                }),
            },
            StreamPart::TextEnd { .. } => {}
            StreamPart::ReasoningStart { id, .. } => {
                if self
                    .reasoning
                    .insert(id.clone(), self.result.content.len())
                    .is_some()
                {
                    return Err(invalid("duplicate reasoning block identity"));
                }
                self.active_reasoning = Some(self.result.content.len());
                self.result.content.push(Content::Reasoning {
                    native: None,
                    text: String::new(),
                    provider_metadata: Default::default(),
                });
            }
            StreamPart::ReasoningDelta { text, source_kind } => {
                let index = match self.active_reasoning {
                    Some(index) => index,
                    None => {
                        let index = self.result.content.len();
                        self.result.content.push(Content::Reasoning {
                            native: None,
                            text: String::new(),
                            provider_metadata: Default::default(),
                        });
                        self.active_reasoning = Some(index);
                        index
                    }
                };
                if *source_kind == Some(ReasoningTextKind::Summary) {
                    // Responses summary and reasoning text can arrive interleaved.
                    // https://developers.openai.com/api/docs/guides/streaming-responses
                    self.reasoning_summaries
                        .entry(index)
                        .or_default()
                        .push_str(text);
                } else if let Some(Content::Reasoning { text: current, .. }) =
                    self.result.content.get_mut(index)
                {
                    current.push_str(text);
                }
            }
            StreamPart::ReasoningEnd {
                id,
                signature,
                native,
            } => {
                if let Some(index) = self.reasoning.get(id).copied() {
                    if self.active_reasoning != Some(index) {
                        return Err(invalid("reasoning end does not match its active block"));
                    }
                    if let Some(summary) = self.reasoning_summaries.remove(&index)
                        && let Some(Content::Reasoning { text, .. }) =
                            self.result.content.get_mut(index)
                    {
                        text.insert_str(0, &summary);
                    }
                    self.active_reasoning = None;
                }
                if let Some(native) = native {
                    let index =
                        self.reasoning.get(id).copied().ok_or_else(|| {
                            invalid("native reasoning block has no opening identity")
                        })?;
                    let part = self
                        .result
                        .content
                        .get_mut(index)
                        .ok_or_else(|| invalid("native reasoning block disappeared"))?;
                    let Content::Reasoning {
                        native: retained, ..
                    } = part
                    else {
                        return Err(invalid("native reasoning does not match a reasoning block"));
                    };
                    *retained = Some(native.clone());
                    bitrouter_ai::protocol::responses::validate_reasoning_projection(part)
                        .map_err(invalid)?;
                }
                if let Some(signature) = signature {
                    let index =
                        self.reasoning.get(id).copied().ok_or_else(|| {
                            invalid("signed reasoning block has no opening identity")
                        })?;
                    if let Some(Content::Reasoning {
                        provider_metadata, ..
                    }) = self.result.content.get_mut(index)
                    {
                        // https://docs.anthropic.com/en/docs/build-with-claude/extended-thinking
                        provider_metadata.insert(
                            "anthropic".into(),
                            serde_json::json!({
                                "signature": signature,
                            }),
                        );
                    }
                }
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
                provider_metadata,
            } => {
                // Gemini complete functionCall frames may omit an ID.
                // https://ai.google.dev/api/generate-content#FunctionCall
                let id = if id.is_empty()
                    && provider_metadata
                        .get("google")
                        .and_then(|value| value.get("functionCallId"))
                        == Some(&serde_json::Value::Null)
                {
                    uuid::Uuid::new_v4().to_string()
                } else if id.is_empty() {
                    return Err(invalid("tool stream fragment has no identity"));
                } else {
                    id.clone()
                };
                let index = match self.tools.get(&id).copied() {
                    Some(index) => index,
                    None => {
                        let index = self.result.content.len();
                        self.result.content.push(Content::ToolCall {
                            id: id.clone(),
                            name: String::new(),
                            arguments: String::new(),
                            provider_executed: false,
                            dynamic: false,
                            provider_metadata: Default::default(),
                        });
                        self.tools.insert(id, index);
                        index
                    }
                };
                if let Content::ToolCall {
                    name: current_name,
                    arguments: current_arguments,
                    provider_metadata: current_metadata,
                    ..
                } = &mut self.result.content[index]
                {
                    if name.is_some()
                        && !current_arguments.is_empty()
                        && serde_json::from_str::<serde_json::Value>(current_arguments).is_ok()
                        && serde_json::from_str::<serde_json::Value>(arguments).is_ok()
                    {
                        return Err(invalid("duplicate complete tool call identity"));
                    }
                    if let Some(name) = name {
                        if !current_name.is_empty() && current_name != name {
                            return Err(invalid("tool name changed within stream"));
                        }
                        current_name.clone_from(name);
                    }
                    for (key, value) in provider_metadata {
                        if current_metadata.get(key).is_some_and(|old| old != value) {
                            return Err(invalid("tool metadata changed within stream"));
                        }
                        current_metadata.insert(key.clone(), value.clone());
                    }
                    current_arguments.push_str(arguments);
                }
            }
            StreamPart::ServerToolCall {
                id,
                name,
                arguments,
                dynamic,
                server_name,
            } => {
                let mut provider_metadata = bitrouter_ai::types::ProviderMetadata::new();
                if let Some(server_name) = server_name {
                    provider_metadata.insert(
                        "anthropic".into(),
                        serde_json::json!({"serverName":server_name}),
                    );
                }
                self.result.content.push(Content::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    provider_executed: true,
                    dynamic: *dynamic,
                    provider_metadata,
                });
            }
            StreamPart::ServerToolResult {
                call_id,
                tool_name,
                output,
                dynamic,
            } => {
                self.result.content.push(Content::ToolResult {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    output: output.clone(),
                    dynamic: *dynamic,
                    provider_metadata: Default::default(),
                });
            }
            StreamPart::File { media_type, data } => self.result.content.push(Content::File {
                media_type: media_type.clone(),
                data: data.clone(),
                filename: None,
                provider_metadata: Default::default(),
            }),
            StreamPart::Source { source } => self.result.content.push(Content::Source {
                source: source.clone(),
                provider_metadata: Default::default(),
            }),
            StreamPart::Usage { .. } => {}
            StreamPart::ResponseStarted { id, .. } => self.result.response_id = Some(id.clone()),
            StreamPart::Finish { reason } => {
                self.terminal = true;
                self.result.finish_reason = Some(reason.clone());
            }
            StreamPart::ResponseCompleted { id, status, .. } => {
                self.terminal = true;
                self.result.response_id = Some(id.clone());
                self.result.finish_reason = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::Length,
                    _ => FinishReason::Error("provider response failed".into()),
                });
            }
        }
        Ok(())
    }
}
