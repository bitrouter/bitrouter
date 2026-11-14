//! Bounded native SSE projection. A terminal remains pending until clean EOF;
//! parseable function arguments alone never close an executable call.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::{
    finish_reason, finish_reason_str, foreign_tool_identity, function_arguments, invalid_response,
    parse_grounding_sources, parse_usage, parts, protocol, render_part, render_usage,
};
use crate::gateway::model::{
    error::Result,
    protocol::{SseEvent, StreamDecoder, StreamEncoder},
    stream::{SseFrame, StreamError},
};
use bitrouter_ai::types::{
    Content, DataContent, FinishReason, ProviderMetadata, Source, StreamPart, ToolCallBoundary,
    Usage, provider_namespace,
};

#[derive(Default)]
pub(super) struct Decoder {
    closed: bool,
    pending: Option<FinishReason>,
    response_id: Option<String>,
    candidate_index: Option<u64>,
    native_parts: parts::Parts,
    buffered: Vec<StreamPart>,
    buffered_bytes: usize,
    source_keys: HashSet<String>,
    source_bytes: usize,
    last_usage: Option<Usage>,
}

impl Decoder {
    fn event(&mut self, chunk: &Value) -> Result<Vec<StreamPart>> {
        if self.closed {
            return Err(invalid_response("GenerateContent stream already closed"));
        }
        if !chunk.is_object() {
            return Err(invalid_response("GenerateContent event must be an object"));
        }
        // Bind identity before admitting even diagnostic usage from this event.
        let mut output = Vec::new();
        if let Some(id) = chunk.get("responseId") {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid_response("invalid GenerateContent response identity"))?;
            match &self.response_id {
                Some(previous) if previous != id => {
                    return Err(invalid_response(
                        "GenerateContent response identity changed",
                    ));
                }
                None => {
                    self.response_id = Some(id.into());
                    output.push(StreamPart::ResponseStarted {
                        id: id.into(),
                        source_protocol: protocol(),
                    });
                }
                _ => (),
            }
        }
        let candidate = match chunk.get("candidates") {
            None => None,
            Some(Value::Array(items)) if items.len() <= 1 => items.first(),
            _ => {
                return Err(invalid_response(
                    "ambiguous GenerateContent stream candidates",
                ));
            }
        };
        if let Some(candidate) = candidate {
            if !candidate.is_object() {
                return Err(invalid_response("invalid GenerateContent stream candidate"));
            }
            let index = match candidate.get("index") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .filter(|index| *index <= i32::MAX as u64)
                    .ok_or_else(|| invalid_response("invalid GenerateContent candidate index"))?,
            };
            match self.candidate_index {
                Some(previous) if previous != index => {
                    return Err(invalid_response("GenerateContent candidate switched"));
                }
                None => self.candidate_index = Some(index),
                _ => (),
            }
        }
        // Only this identity- and history-qualified snapshot may accompany
        // either a successful observation or any diagnostic/refusal below.
        let usage = chunk
            .get("usageMetadata")
            .map(|raw| self.checked_usage(raw))
            .transpose()?;
        let failure =
            |message: &'static str| crate::gateway::model::error::ModelError::InvalidResponse {
                message: message.into(),
                usage: usage.clone().map(Box::new),
            };
        if chunk
            .get("promptFeedback")
            .is_some_and(|feedback| !feedback.is_object())
        {
            return Err(failure("invalid GenerateContent prompt feedback"));
        }
        if candidate
            .is_some_and(|candidate| candidate.get("error").is_some_and(|error| !error.is_null()))
        {
            return Err(failure("GenerateContent stream candidate reports failure"));
        }
        if chunk.get("error").is_some_and(|error| !error.is_null())
            || chunk
                .get("promptFeedback")
                .and_then(|feedback| feedback.get("blockReason"))
                .is_some_and(|reason| !reason.is_null())
        {
            return Err(failure("GenerateContent stream reports failure"));
        }
        let mut event_parts = Vec::new();
        if let Some(candidate) = candidate {
            if self.pending.is_some() {
                return Err(invalid_response(
                    "GenerateContent candidate follows terminal",
                ));
            }
            if let Some(content) = candidate.get("content") {
                if content
                    .get("role")
                    .is_some_and(|role| role.as_str() != Some("model"))
                {
                    return Err(failure("invalid GenerateContent output role"));
                }
                let values = content
                    .as_object()
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| failure("invalid GenerateContent stream content"))?;
                let decoded = self
                    .native_parts
                    .decode(values)
                    .map_err(|_| failure("unsupported GenerateContent stream part"))?;
                for content in decoded {
                    let part = match content {
                        Content::Text {
                            text,
                            provider_metadata,
                        } => {
                            no_unrepresentable_signature(&provider_metadata).map_err(|_| {
                                failure("signed text or media requires native wire custody")
                            })?;
                            StreamPart::TextDelta { text }
                        }
                        Content::Reasoning {
                            text,
                            provider_metadata,
                            ..
                        } => {
                            no_unrepresentable_signature(&provider_metadata).map_err(|_| {
                                failure("signed text or media requires native wire custody")
                            })?;
                            StreamPart::ReasoningDelta {
                                text,
                                source_kind: None,
                            }
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            provider_metadata,
                            ..
                        } => StreamPart::ToolCallDelta {
                            id,
                            name: Some(name),
                            arguments,
                            provider_metadata,
                        },
                        Content::File {
                            media_type,
                            data,
                            provider_metadata,
                            ..
                        } => {
                            no_unrepresentable_signature(&provider_metadata).map_err(|_| {
                                failure("signed text or media requires native wire custody")
                            })?;
                            StreamPart::File { media_type, data }
                        }
                        _ => {
                            return Err(failure("unsupported GenerateContent stream output"));
                        }
                    };
                    event_parts.push(part);
                }
            }
            for content in parse_grounding_sources(candidate.get("groundingMetadata")) {
                if let Content::Source { source, .. } = content {
                    let key = match &source {
                        Source::Url { url, .. } => url,
                        Source::Document { id, .. } => id,
                    };
                    if !self.source_keys.contains(key) {
                        if self.source_keys.len() >= parts::MAX_CALLS
                            || key.len() > parts::MAX_BYTES
                        {
                            return Err(invalid_response(
                                "GenerateContent grounding exceeds codec bound",
                            ));
                        }
                        self.source_bytes = self
                            .source_bytes
                            .checked_add(
                                bitrouter_ai::wire::json(&source, parts::MAX_BYTES)
                                    .map_err(|_| {
                                        invalid_response(
                                            "GenerateContent grounding exceeds codec bound",
                                        )
                                    })?
                                    .len(),
                            )
                            .filter(|size| *size <= parts::MAX_BYTES)
                            .ok_or_else(|| {
                                invalid_response("GenerateContent grounding exceeds codec bound")
                            })?;
                        self.source_keys.insert(key.clone());
                        event_parts.push(StreamPart::Source { source });
                    }
                }
            }
            if let Some(raw_reason) = candidate.get("finishReason") {
                self.pending = Some(
                    raw_reason
                        .as_str()
                        .and_then(finish_reason)
                        .ok_or_else(|| failure("invalid GenerateContent terminal"))?,
                );
            }
        }
        // Nothing from a malformed event is yielded. Buffer the suffix starting
        // at its first call until the actual source turn is complete at EOF.
        for part in event_parts {
            if !self.buffered.is_empty() || matches!(part, StreamPart::ToolCallDelta { .. }) {
                let size = bitrouter_ai::wire::json(&part, parts::MAX_BYTES)
                    .map_err(|_| {
                        invalid_response("GenerateContent stream suffix exceeds codec bound")
                    })?
                    .len();
                self.buffered_bytes = self
                    .buffered_bytes
                    .checked_add(size)
                    .filter(|size| *size <= parts::MAX_BYTES)
                    .ok_or_else(|| {
                        invalid_response("GenerateContent stream suffix exceeds codec bound")
                    })?;
                self.buffered.push(part);
            } else {
                output.push(part);
            }
        }
        if let Some(usage) = usage {
            self.last_usage = Some(usage.clone());
            output.push(StreamPart::Usage { usage });
        }
        Ok(output)
    }
    fn checked_usage(&self, raw: &Value) -> Result<Usage> {
        let usage = parse_usage(raw)
            .ok_or_else(|| invalid_response("GenerateContent stream usage is not usable"))?;
        if self.last_usage.as_ref().is_some_and(|previous| {
            usage.prompt_tokens < previous.prompt_tokens
                || usage.completion_tokens < previous.completion_tokens
                || usage.reasoning_tokens < previous.reasoning_tokens
                || usage.cache_read_tokens < previous.cache_read_tokens
        }) {
            return Err(invalid_response(
                "GenerateContent cumulative usage decreased",
            ));
        }
        Ok(usage)
    }
}

fn no_unrepresentable_signature(metadata: &ProviderMetadata) -> Result<()> {
    if provider_namespace(metadata, "google")
        .is_some_and(|fields| fields.contains_key("thoughtSignature"))
    {
        return Err(invalid_response(
            "signed text or media requires native wire custody",
        ));
    }
    Ok(())
}

impl StreamDecoder for Decoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let result = if event.event.as_deref().is_some_and(|name| name != "message") {
            Err(invalid_response("unexpected GenerateContent stream event"))
        } else if event.data.len() > parts::MAX_BYTES {
            Err(invalid_response(
                "GenerateContent stream event exceeds codec bound",
            ))
        } else if event.data.trim().is_empty() && !self.closed {
            Ok(Vec::new())
        } else {
            super::native::value(&event.data).and_then(|chunk| self.event(&chunk))
        };
        if result.is_err() {
            self.closed = true;
            self.pending = None;
            self.buffered.clear();
        }
        result
    }

    fn pending_terminal(&self) -> Option<StreamPart> {
        self.pending
            .clone()
            .map(|reason| StreamPart::Finish { reason })
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>> {
        if self.closed {
            return Err(invalid_response(
                "GenerateContent stream is not a clean terminal",
            ));
        }
        self.closed = true;
        let reason = self
            .pending
            .take()
            .ok_or_else(|| invalid_response("GenerateContent stream ended without terminal"))?;
        if !self.buffered.is_empty()
            && !matches!(reason, FinishReason::Stop | FinishReason::ToolCalls)
        {
            self.buffered.clear();
            return Err(invalid_response(
                "GenerateContent function call turn did not complete",
            ));
        }
        let mut output = Vec::new();
        for part in self.buffered.drain(..) {
            let call = match &part {
                StreamPart::ToolCallDelta { id, .. } => Some(id.clone()),
                _ => None,
            };
            output.push(part);
            if let Some(id) = call {
                output.push(StreamPart::ToolCallEnd {
                    id,
                    boundary: ToolCallBoundary::Turn { complete: true },
                });
            }
        }
        output.push(StreamPart::Finish { reason });
        Ok(output)
    }
}

#[derive(Default)]
pub(super) struct Encoder {
    calls: Vec<ToolFragments>,
    seen: HashSet<String>,
    completed_responses: HashSet<String>,
    bytes: usize,
    closed: bool,
    failed: bool,
}
struct ToolFragments {
    id: String,
    name: String,
    arguments: String,
    metadata: ProviderMetadata,
    closed: bool,
    responses: bool,
}

impl Encoder {
    fn append(
        &mut self,
        id: &str,
        name: Option<&str>,
        arguments: &str,
        metadata: &ProviderMetadata,
    ) -> Result<()> {
        if id.is_empty() || foreign_tool_identity(metadata) {
            return Err(invalid_response(
                "unsupported GenerateContent tool identity",
            ));
        }
        let metadata_size = bitrouter_ai::wire::json(metadata, parts::MAX_BYTES)
            .map_err(|_| invalid_response("GenerateContent tool metadata exceeds codec bound"))?
            .len();
        let size = id
            .len()
            .checked_add(name.map_or(0, str::len))
            .and_then(|size| size.checked_add(arguments.len()))
            .and_then(|size| size.checked_add(metadata_size))
            .ok_or_else(|| invalid_response("GenerateContent tool exceeds codec bound"))?;
        self.bytes = self
            .bytes
            .checked_add(size)
            .filter(|size| *size <= parts::MAX_BYTES)
            .ok_or_else(|| invalid_response("GenerateContent tools exceed codec bound"))?;
        if let Some(call) = self.calls.iter_mut().find(|call| call.id == id) {
            if call.closed
                || name.is_some_and(|name| name != call.name)
                || (!metadata.is_empty() && metadata != &call.metadata)
            {
                return Err(invalid_response(
                    "GenerateContent call identity or metadata changed",
                ));
            }
            call.arguments.push_str(arguments);
        } else {
            if self.seen.len() >= parts::MAX_CALLS || !self.seen.insert(id.into()) {
                return Err(invalid_response(
                    "duplicate or excessive GenerateContent calls",
                ));
            }
            let name = name
                .filter(|name| !name.is_empty())
                .ok_or_else(|| invalid_response("GenerateContent call name missing"))?;
            self.calls.push(ToolFragments {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
                metadata: metadata.clone(),
                closed: false,
                responses: false,
            });
        }
        Ok(())
    }

    fn boundary(&mut self, id: &str, boundary: &ToolCallBoundary) -> Result<Vec<Value>> {
        let confirmation = matches!(boundary,ToolCallBoundary::ResponsesItem {
            status:Some(status),terminal_only:true
        } if status=="completed");
        // A Responses item can complete before its terminal snapshot is
        // confirmed at EOF. Retained IDs are bounded by the same call quota;
        // confirmation cannot reopen or emit the already projected call.
        if self.completed_responses.contains(id) {
            return if confirmation {
                Ok(Vec::new())
            } else {
                Err(invalid_response(
                    "conflicting GenerateContent call confirmation",
                ))
            };
        }
        let call = self
            .calls
            .iter_mut()
            .find(|call| call.id == id)
            .ok_or_else(|| invalid_response("GenerateContent call boundary has no open call"))?;
        if call.closed {
            return if call.responses && confirmation {
                Ok(Vec::new())
            } else {
                Err(invalid_response("duplicate GenerateContent call boundary"))
            };
        }
        if function_arguments(&call.arguments).is_none() {
            return Err(invalid_response(
                "GenerateContent arguments incomplete at call boundary",
            ));
        }
        match boundary {
            ToolCallBoundary::Arguments => return Ok(Vec::new()),
            // Payload completion without status does not prove a successful
            // turn. The concrete terminal-only boundary must still arrive.
            ToolCallBoundary::ResponsesItem {
                status: None,
                terminal_only: false,
            } => return Ok(Vec::new()),
            ToolCallBoundary::Turn { complete: true }
            | ToolCallBoundary::MessagesBlock { complete: true } => (),
            ToolCallBoundary::ResponsesItem {
                status: Some(status),
                ..
            } if status == "completed" => call.responses = true,
            _ => {
                return Err(invalid_response(
                    "GenerateContent source call did not complete",
                ));
            }
        }
        call.closed = true;
        self.flush()
    }

    fn flush(&mut self) -> Result<Vec<Value>> {
        let count = self.calls.iter().take_while(|call| call.closed).count();
        let output = self
            .calls
            .iter()
            .take(count)
            .map(|call| {
                render_part(&Content::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: call.metadata.clone(),
                })?
                .map(candidate_part)
                .ok_or_else(|| invalid_response("missing GenerateContent call projection"))
            })
            .collect::<Result<Vec<_>>>()?;
        for call in self.calls.drain(..count) {
            if call.responses {
                self.completed_responses.insert(call.id);
            }
        }
        Ok(output)
    }

    fn ordered(&self) -> Result<()> {
        if self.calls.is_empty() {
            Ok(())
        } else {
            Err(invalid_response(
                "content interleaves an incomplete GenerateContent call",
            ))
        }
    }

    fn event(&mut self, part: &StreamPart) -> Result<Vec<Value>> {
        if self.closed {
            return Err(invalid_response("GenerateContent stream already terminal"));
        }
        match part {
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
                provider_metadata,
            } => {
                self.append(id, name.as_deref(), arguments, provider_metadata)?;
                Ok(Vec::new())
            }
            StreamPart::ToolCallEnd { id, boundary } => self.boundary(id, boundary),
            StreamPart::ResponseStarted { .. } => Ok(Vec::new()),
            StreamPart::TextStart { .. }
            | StreamPart::TextEnd { .. }
            | StreamPart::ReasoningStart { .. } => {
                self.ordered()?;
                // Native parts are whole values rather than block-framed events.
                Ok(Vec::new())
            }
            StreamPart::ServerToolCall { .. } | StreamPart::ServerToolResult { .. } => Err(
                invalid_response("provider-executed tool has no native client call projection"),
            ),
            StreamPart::ReasoningEnd {
                signature, native, ..
            } => {
                if signature.is_some() || native.is_some() {
                    return Err(invalid_response(
                        "foreign signed reasoning has no GenerateContent projection",
                    ));
                }
                Ok(Vec::new())
            }
            StreamPart::TextDelta { text } => {
                self.ordered()?;
                Ok(vec![candidate_part(json!({"text":text}))])
            }
            StreamPart::ReasoningDelta { text, source_kind } => {
                self.ordered()?;
                if source_kind.is_some() {
                    return Err(invalid_response(
                        "foreign reasoning identity has no GenerateContent projection",
                    ));
                }
                Ok(vec![candidate_part(json!({"text":text,"thought":true}))])
            }
            StreamPart::File { media_type, data } => {
                self.ordered()?;
                Ok(vec![candidate_part(match data {
                    DataContent::Base64 { data } => {
                        json!({"inlineData":{"mimeType":media_type,"data":data}})
                    }
                    DataContent::Url { url } => {
                        json!({"fileData":{"mimeType":media_type,"fileUri":url}})
                    }
                })])
            }
            StreamPart::Source { source } => {
                self.ordered()?;
                match source {
                    Source::Url { url, title, .. } => Ok(vec![
                        json!({"candidates":[{"groundingMetadata":{"groundingChunks":[{"web":{"uri":url,"title":title}}]}}]}),
                    ]),
                    Source::Document { .. } => Err(invalid_response(
                        "document source has no GenerateContent projection",
                    )),
                }
            }
            StreamPart::Usage { usage } => Ok(vec![json!({"usageMetadata":render_usage(usage)?})]),
            StreamPart::Finish { reason } => {
                self.ordered()?;
                if matches!(reason, FinishReason::Error(_)) {
                    return Err(invalid_response("unsuccessful source terminal"));
                }
                self.closed = true;
                Ok(vec![
                    json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":finish_reason_str(reason)}]}),
                ])
            }
            StreamPart::ResponseCompleted { status, usage, .. } => {
                self.ordered()?;
                if !matches!(status.as_str(), "completed" | "incomplete") {
                    return Err(invalid_response("unsuccessful source terminal"));
                }
                let mut value = json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":if status == "completed" {"STOP"} else {"MAX_TOKENS"}}]});
                if let Some(usage) = usage {
                    value["usageMetadata"] = render_usage(usage)?;
                }
                self.closed = true;
                Ok(vec![value])
            }
        }
    }
}

fn candidate_part(part: Value) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":[part]}}]})
}
fn frame(value: Value) -> SseFrame {
    SseFrame::Event {
        event: None,
        data: value.to_string(),
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let result = self.event(part);
        if result.is_err() {
            self.closed = true;
            self.failed = true;
            self.calls.clear();
        }
        result.map(|values| values.into_iter().map(frame).collect())
    }
    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        self.ordered()?;
        if self.failed {
            return Err(invalid_response("GenerateContent stream projection failed"));
        }
        if !self.closed {
            return Err(invalid_response("GenerateContent output has no terminal"));
        }
        Ok(Vec::new())
    }
    fn encode_error(&mut self, message: &str) -> Vec<SseFrame> {
        self.closed = true;
        self.failed = true;
        self.calls.clear();
        vec![frame(
            json!({"error":{"code":502,"status":"UNAVAILABLE","message":message}}),
        )]
    }
    fn encode_stream_error(&mut self, error: &StreamError) -> Vec<SseFrame> {
        self.closed = true;
        self.failed = true;
        self.calls.clear();
        vec![frame(
            json!({"error":{"code":error.status,"status":if error.status == 429 {"RESOURCE_EXHAUSTED"} else {"UNAVAILABLE"},"message":error.message,"details":[{"reason":error.code}]}}),
        )]
    }
}
