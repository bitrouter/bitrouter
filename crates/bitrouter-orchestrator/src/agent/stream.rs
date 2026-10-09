//! SDK streaming and bounded collection of complete or interrupted assistant Items.

use std::collections::HashMap;

use bitrouter_ai::types::{
    Content, FinishReason, GenerateResult, Message, Prompt, Role, StreamPart, Usage,
};
use bitrouter_sdk::language_model::PipelineResponse;
use futures::StreamExt;
use tokio::sync::mpsc;

use super::{Agent, RunEvent};

const MAX_MODEL_CONTENT_BYTES: usize = 512 * 1024;
const MAX_LIVE_DELTAS: usize = 8_192;

#[derive(Default)]
pub(super) struct StreamCollector {
    content: Vec<Content>,
    tool_indices: HashMap<String, usize>,
    pub(super) usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    response_id: Option<String>,
    content_bytes: usize,
    live_deltas: usize,
    pub(super) request_id: Option<String>,
}

pub(super) struct AssistantAttempt {
    pub(super) step_id: String,
    pub(super) item_id: String,
    pub(super) collector: StreamCollector,
}

impl StreamCollector {
    pub(super) async fn observe(
        &mut self,
        part: StreamPart,
        item_id: &str,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<(), String> {
        let incoming = match &part {
            StreamPart::TextDelta { text } | StreamPart::ReasoningDelta { text, .. } => text.len(),
            StreamPart::ToolCallDelta { arguments, .. } => arguments.len(),
            _ => 0,
        };
        if self.content_bytes.saturating_add(incoming) > MAX_MODEL_CONTENT_BYTES {
            return Err("model turn exceeded the 512 KiB content limit".into());
        }
        match part {
            StreamPart::TextStart { .. } => self.content.push(Content::Text {
                text: String::new(),
                provider_metadata: Default::default(),
            }),
            StreamPart::TextDelta { text } => {
                self.content_bytes = self.content_bytes.saturating_add(text.len());
                if let Some(Content::Text { text: current, .. }) = self.content.last_mut() {
                    current.push_str(&text);
                } else {
                    self.content.push(Content::Text {
                        text: text.clone(),
                        provider_metadata: Default::default(),
                    });
                }
                if let Some(events) = events
                    && self.live_deltas < MAX_LIVE_DELTAS
                {
                    let _ = events
                        .send(RunEvent::AssistantDelta {
                            item_id: item_id.into(),
                            text,
                        })
                        .await;
                    self.live_deltas += 1;
                }
            }
            StreamPart::TextEnd { .. } => {}
            StreamPart::ReasoningStart { .. } => self.content.push(Content::Reasoning {
                native: None,
                text: String::new(),
                provider_metadata: Default::default(),
            }),
            StreamPart::ReasoningDelta { text, .. } => {
                self.content_bytes = self.content_bytes.saturating_add(text.len());
                if let Some(Content::Reasoning { text: current, .. }) = self.content.last_mut() {
                    current.push_str(&text);
                } else {
                    self.content.push(Content::Reasoning {
                        native: None,
                        text,
                        provider_metadata: Default::default(),
                    });
                }
            }
            StreamPart::ReasoningEnd {
                signature, native, ..
            } => {
                if let Some(Content::Reasoning {
                    provider_metadata,
                    native: retained,
                    ..
                }) = self.content.last_mut()
                {
                    *retained = native;
                    if let Some(signature) = signature {
                        provider_metadata.insert(
                            "anthropic".into(),
                            serde_json::json!({"signature": signature}),
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
                self.content_bytes = self.content_bytes.saturating_add(arguments.len());
                if let Some(index) = self.tool_indices.get(&id)
                    && name.is_some()
                    && let Content::ToolCall {
                        arguments: previous,
                        ..
                    } = &self.content[*index]
                    && serde_json::from_str::<serde_json::Value>(previous).is_ok()
                    && serde_json::from_str::<serde_json::Value>(&arguments).is_ok()
                {
                    return Err("duplicate complete tool call ID in provider stream".into());
                }
                let index = match self.tool_indices.get(&id).copied() {
                    Some(index) => index,
                    None => {
                        let index = self.content.len();
                        self.content.push(Content::ToolCall {
                            id: id.clone(),
                            name: name.clone().unwrap_or_default(),
                            arguments: String::new(),
                            provider_executed: false,
                            dynamic: false,
                            provider_metadata,
                        });
                        self.tool_indices.insert(id, index);
                        index
                    }
                };
                if let Content::ToolCall {
                    name: current_name,
                    arguments: current_arguments,
                    ..
                } = &mut self.content[index]
                {
                    if let Some(name) = name
                        && current_name.is_empty()
                    {
                        *current_name = name;
                    }
                    current_arguments.push_str(&arguments);
                }
            }
            StreamPart::ServerToolCall { .. } | StreamPart::ServerToolResult { .. } => {
                return Err("native model turn unexpectedly invoked an SDK server tool".into());
            }
            StreamPart::File { media_type, data } => self.content.push(Content::File {
                media_type,
                data,
                filename: None,
                provider_metadata: Default::default(),
            }),
            StreamPart::Source { source } => self.content.push(Content::Source {
                source,
                provider_metadata: Default::default(),
            }),
            StreamPart::Usage { usage } => self.usage = Some(usage),
            StreamPart::ResponseStarted { id, .. } => self.response_id = Some(id),
            StreamPart::Finish { reason } => self.finish_reason = Some(reason),
            StreamPart::ResponseCompleted {
                id, status, usage, ..
            } => {
                self.response_id = Some(id);
                if usage.is_some() {
                    self.usage = usage;
                }
                self.finish_reason = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::Length,
                    other => FinishReason::Error(format!("response ended with status {other}")),
                });
            }
        }
        if self.content_bytes > MAX_MODEL_CONTENT_BYTES {
            return Err("model turn exceeded the 512 KiB content limit".into());
        }
        Ok(())
    }

    pub(super) fn partial(&self) -> Message {
        let mut message = Message {
            role: Role::Assistant,
            content: Vec::new(),
        };
        for part in &self.content {
            message.content.push(part.clone());
            if serde_json::to_vec(&message)
                .map_or(true, |value| value.len() > MAX_MODEL_CONTENT_BYTES)
            {
                message.content.pop();
                break;
            }
        }
        message
    }

    pub(super) fn finish(&self, request_id: String) -> Result<PipelineResponse, String> {
        match self.finish_reason.as_ref() {
            Some(FinishReason::Stop | FinishReason::ToolCalls) => {}
            Some(reason) => return Err(format!("model turn did not complete safely: {reason:?}")),
            None => return Err("model stream ended without a finish reason".into()),
        }
        let mut content = self.content.clone();
        content.retain(|part| {
            !matches!(
                part,
                Content::Text { text, .. } | Content::Reasoning { text, native: None, .. } if text.is_empty()
            )
        });
        Ok(PipelineResponse {
            request_id,
            result: GenerateResult {
                content,
                usage: self.usage.clone(),
                finish_reason: self.finish_reason.clone(),
                response_id: self.response_id.clone(),
                stop_details: None,
                provider_metadata: Default::default(),
            }
            .into(),
        })
    }
}

impl Agent {
    pub(super) async fn execute_turn(
        &self,
        prompt: Prompt,
        attempt: &mut AssistantAttempt,
        events: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<PipelineResponse, String> {
        let (request_id, mut stream) = self
            .app
            .execute_native_stream(prompt, self.caller.clone())
            .await
            .map_err(|error| error.to_string())?;
        attempt.collector.request_id = Some(request_id.clone());
        while let Some(part) = stream.next().await {
            attempt
                .collector
                .observe(
                    part.map_err(|error| error.to_string())?,
                    &attempt.item_id,
                    events,
                )
                .await?;
        }
        attempt.collector.finish(request_id)
    }
}
