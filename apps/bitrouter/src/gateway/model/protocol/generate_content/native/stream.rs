//! Authenticated native frames travel under private custody, outside canonical conversion.
//! Only genuine observations are exposed to ordinary stream hooks. Rewriting them
//! cannot silently restore original text; terminal frames wait for clean EOF and
//! the existing delivery owner. No raw function call becomes an executable tool.
use super::{lifecycle::Lifecycle, value};
use crate::gateway::model::{
    error::{ModelError, Result},
    protocol::{SseEvent, StreamDecoder, StreamEncoder},
    stream::{SseFrame, StreamError},
};
use bitrouter_ai::types::{FinishReason, StreamPart};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

fn invalid() -> ModelError {
    ModelError::InvalidResponse {
        message: "native stream custody mismatch".into(),
        usage: None,
    }
}
struct Packet {
    frame: SseFrame,
    expected: VecDeque<StreamPart>,
    held: bool,
    accepted: bool,
}
struct State {
    packets: VecDeque<Packet>,
    remaining: usize,
    terminal: Option<FinishReason>,
    delivered: bool,
    failed: bool,
}
pub(crate) struct Custody {
    state: Mutex<State>,
    ready: tokio::sync::Notify,
}
impl Custody {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                packets: VecDeque::new(),
                remaining: limit,
                terminal: None,
                delivered: false,
                failed: false,
            }),
            ready: tokio::sync::Notify::new(),
        })
    }
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| invalid())
    }
    fn push(&self, event: &SseEvent, expected: &[StreamPart], held: bool) -> Result<()> {
        let mut state = self.state()?;
        if state.failed
            || state.terminal.is_some()
            || state.packets.back().is_some_and(|packet| !packet.accepted)
        {
            return Err(invalid());
        }
        let observations = expected
            .iter()
            .try_fold(0usize, |total, part| {
                serde_json::to_vec(part)
                    .ok()
                    .and_then(|bytes| total.checked_add(bytes.len()))
            })
            .ok_or_else(invalid)?;
        state.remaining = state
            .remaining
            .checked_sub(
                event
                    .data
                    .len()
                    .saturating_add(observations)
                    .saturating_add(128),
            )
            .ok_or_else(invalid)?;
        state.packets.push_back(Packet {
            frame: SseFrame::Event {
                event: event.event.clone(),
                data: event.data.clone(),
            },
            expected: expected.iter().cloned().collect(),
            held,
            accepted: false,
        });
        Ok(())
    }
    fn complete(&self, reason: FinishReason) -> Result<()> {
        let mut state = self.state()?;
        if state.failed
            || state.terminal.is_some()
            || state.packets.iter().any(|packet| !packet.accepted)
        {
            return Err(invalid());
        }
        state.terminal = Some(reason);
        Ok(())
    }

    /// Wake only after the actual attempt owner accepted original evidence and
    /// operational observations. This is not a canonical text/usage marker.
    pub(crate) async fn ready(&self) {
        self.ready.notified().await;
    }

    pub(crate) fn drain_ready(&self) -> Result<Vec<SseFrame>> {
        let mut state = self.state()?;
        if state.failed || state.delivered {
            return Err(invalid());
        }
        let mut frames = Vec::new();
        while state
            .packets
            .front()
            .is_some_and(|packet| packet.accepted && packet.expected.is_empty() && !packet.held)
        {
            if let Some(packet) = state.packets.pop_front() {
                frames.push(packet.frame);
            }
        }
        Ok(frames)
    }
}

pub(crate) struct Decoder {
    custody: Arc<Custody>,
    lifecycle: Lifecycle,
    response_seen: bool,
    closed: bool,
}
impl Decoder {
    pub(crate) fn new(custody: Arc<Custody>, limit: usize) -> Self {
        Self {
            custody,
            lifecycle: Lifecycle::new(limit),
            response_seen: false,
            closed: false,
        }
    }
    pub(crate) fn event(&mut self, event: &SseEvent) -> Result<(Value, Vec<StreamPart>)> {
        let result = self.accepted_event(event);
        if result.is_err() {
            self.closed = true;
            if let Ok(mut state) = self.custody.state() {
                state.failed = true;
                state.packets.clear();
            }
        }
        result
    }

    pub(crate) fn accepted(&mut self) -> Result<()> {
        let mut state = self.custody.state()?;
        if state.failed || state.terminal.is_some() {
            return Err(invalid());
        }
        let packet = state
            .packets
            .back_mut()
            .filter(|packet| !packet.accepted)
            .ok_or_else(invalid)?;
        packet.accepted = true;
        drop(state);
        self.custody.ready.notify_one();
        Ok(())
    }

    fn accepted_event(&mut self, event: &SseEvent) -> Result<(Value, Vec<StreamPart>)> {
        if self.closed {
            return Err(invalid());
        }
        if event.event.as_deref().is_some_and(|name| name != "message") {
            return Err(invalid());
        }
        let body = value(&event.data)?;
        if body.get("type").and_then(Value::as_str) == Some("error") {
            return Err(invalid());
        }
        let held = self.lifecycle.observe(&body)?;
        let mut observed = Vec::new();
        if !self.response_seen
            && let Some(id) = body.get("responseId").and_then(Value::as_str)
        {
            self.response_seen = true;
            observed.push(StreamPart::ResponseStarted {
                id: id.into(),
                source_protocol: super::super::protocol(),
            });
        }
        if let Some(candidate) = self.lifecycle.primary_candidate(&body)
            && let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    observed.push(
                        if part.get("thought").and_then(Value::as_bool) == Some(true) {
                            StreamPart::ReasoningDelta {
                                text: text.into(),
                                source_kind: None,
                            }
                        } else {
                            StreamPart::TextDelta { text: text.into() }
                        },
                    );
                }
            }
        }
        self.custody.push(event, &observed, held)?;
        if let Some(usage) = body
            .get("usageMetadata")
            .and_then(super::super::parse_usage)
        {
            observed.push(StreamPart::Usage { usage });
        }
        Ok((body, observed))
    }
}
impl StreamDecoder for Decoder {
    fn decode(&mut self, event: &SseEvent) -> Result<Vec<StreamPart>> {
        let (_, parts) = self.event(event)?;
        self.accepted()?;
        Ok(parts)
    }
    fn finish(&mut self) -> Result<Vec<StreamPart>> {
        if self.closed {
            return Err(invalid());
        }
        let reason = super::super::finish_reason(self.lifecycle.complete()?).ok_or_else(invalid)?;
        self.closed = true;
        self.custody.complete(reason.clone())?;
        Ok(vec![StreamPart::Finish { reason }])
    }
}

pub(crate) struct Encoder {
    custody: Arc<Custody>,
}
impl Encoder {
    pub(crate) fn new(custody: Arc<Custody>) -> Self {
        Self { custody }
    }
}
impl StreamEncoder for Encoder {
    fn encode(&mut self, part: &StreamPart) -> Result<Vec<SseFrame>> {
        let mut state = self.custody.state()?;
        if state.failed || state.delivered {
            return Err(invalid());
        }
        // Canonical estimates or normalized counts never overwrite raw native usage.
        if matches!(part, StreamPart::Usage { .. }) {
            return Ok(Vec::new());
        }
        let terminal = if let StreamPart::Finish { reason } = part {
            if state.terminal.as_ref() != Some(reason)
                || state
                    .packets
                    .iter()
                    .any(|packet| !packet.accepted || !packet.expected.is_empty())
            {
                return Err(invalid());
            }
            state.delivered = true;
            true
        } else {
            let expected = state
                .packets
                .iter_mut()
                .find_map(|packet| {
                    packet
                        .expected
                        .front()
                        .cloned()
                        .map(|expected| (packet, expected))
                })
                .ok_or_else(invalid)?;
            if &expected.1 != part {
                state.failed = true;
                return Err(invalid());
            }
            if !expected.0.accepted {
                return Err(invalid());
            }
            expected.0.expected.pop_front();
            false
        };
        let mut output = Vec::new();
        while state.packets.front().is_some_and(|packet| {
            packet.accepted && packet.expected.is_empty() && (terminal || !packet.held)
        }) {
            if let Some(packet) = state.packets.pop_front() {
                output.push(packet.frame);
            }
        }
        Ok(output)
    }
    fn encode_stream_error(&mut self, error: &StreamError) -> Vec<SseFrame> {
        if let Ok(mut state) = self.custody.state() {
            state.failed = true;
            state.packets.clear();
        }
        vec![SseFrame::Event{event:None,data:serde_json::json!({"error":{"message":error.message,"code":error.status,"status":error.code}}).to_string()}]
    }
    fn finish(&mut self) -> Result<Vec<SseFrame>> {
        let state = self.custody.state()?;
        if !state.delivered || !state.packets.is_empty() || state.failed {
            return Err(invalid());
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
    fn event(body: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: body.to_string(),
        }
    }
    fn observe(
        decoder: &mut Decoder,
        encoder: &mut Encoder,
        event: &SseEvent,
    ) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        for part in decoder.decode(event)? {
            frames.extend(encoder.encode(&part)?);
        }
        Ok(frames)
    }

    #[test]
    fn native_generate_content_raw_stream_keeps_signed_media_full_arrays_until_eof() -> TestResult {
        let custody = Custody::new(65536);
        let mut decoder = Decoder::new(custody.clone(), 65536);
        let mut encoder = Encoder::new(custody);
        let first = event(
            json!({"responseId":"raw-id","candidates":[{"index":5,"content":{"parts":[{"text":"signed","thoughtSignature":"opaque-signature"}]}}]}),
        );
        let frames = observe(&mut decoder, &mut encoder, &first)?;
        assert_eq!(
            frames,
            vec![SseFrame::Event {
                event: None,
                data: first.data
            }]
        );
        let terminal = event(json!({"candidates":[
            {"index":5,"finishReason":"STOP","content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}},
                {"functionCall":{"id":"original-call","name":"native-only","args":{"opaque":[1.5,null]}}}]}},
            {"index":9,"content":{"parts":[{"text":"second-open"}]}}
        ]}));
        assert!(observe(&mut decoder, &mut encoder, &terminal)?.is_empty());
        let open = event(
            json!({"promptFeedback":{"safetyRatings":[]},"candidates":[{"index":9,"content":{"parts":[{"text":"second-final"}]},"finishReason":"MAX_TOKENS"}]}),
        );
        assert!(observe(&mut decoder, &mut encoder, &open)?.is_empty());
        let usage = event(
            json!({"error":null,"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3}}),
        );
        assert!(observe(&mut decoder, &mut encoder, &usage)?.is_empty());
        assert!(encoder.finish().is_err());
        let mut output = Vec::new();
        for part in decoder.finish()? {
            assert!(matches!(part, StreamPart::Finish { .. }));
            output.extend(encoder.encode(&part)?);
        }
        assert_eq!(
            output,
            vec![
                SseFrame::Event {
                    event: None,
                    data: terminal.data
                },
                SseFrame::Event {
                    event: None,
                    data: open.data
                },
                SseFrame::Event {
                    event: None,
                    data: usage.data
                }
            ]
        );
        assert!(encoder.finish()?.is_empty());
        Ok(())
    }

    #[test]
    fn native_generate_content_raw_stream_error_names_never_become_success() -> TestResult {
        for (name, body) in [
            (
                Some("error"),
                json!({"candidates":[{"finishReason":"STOP"}]}),
            ),
            (
                Some("unknown"),
                json!({"candidates":[{"finishReason":"STOP"}]}),
            ),
            (
                None,
                json!({"type":"error","candidates":[{"finishReason":"STOP"}]}),
            ),
        ] {
            let custody = Custody::new(65536);
            let mut decoder = Decoder::new(custody.clone(), 65536);
            let mut encoder = Encoder::new(custody);
            let event = SseEvent {
                event: name.map(str::to_owned),
                data: body.to_string(),
            };
            assert!(decoder.decode(&event).is_err());
            assert!(decoder.finish().is_err());
            assert!(encoder.finish().is_err());
        }
        Ok(())
    }

    #[test]
    fn native_generate_content_raw_media_requires_actual_acceptance_without_fake_observations()
    -> TestResult {
        let custody = Custody::new(65536);
        let mut decoder = Decoder::new(custody.clone(), 65536);
        let frame = event(
            json!({"candidates":[{"index":0,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]}}]}),
        );
        let (_, observations) = decoder.event(&frame)?;
        assert!(observations.is_empty());
        assert!(custody.drain_ready()?.is_empty());
        decoder.accepted()?;
        assert_eq!(
            custody.drain_ready()?,
            vec![SseFrame::Event {
                event: None,
                data: frame.data
            }]
        );
        assert!(custody.drain_ready()?.is_empty());
        let terminal = event(json!({"candidates":[{"index":0,"finishReason":"STOP"}]}));
        assert!(decoder.decode(&terminal)?.is_empty());
        assert!(custody.drain_ready()?.is_empty());
        let mut encoder = Encoder::new(custody);
        assert!(encoder.finish().is_err());
        let parts = decoder.finish()?;
        assert_eq!(parts.len(), 1);
        assert_eq!(
            encoder.encode(&parts[0])?,
            vec![SseFrame::Event {
                event: None,
                data: terminal.data
            }]
        );
        Ok(())
    }

    #[test]
    fn native_generate_content_raw_stream_invalid_tail_or_hook_rewrite_cannot_flush() -> TestResult
    {
        let custody = Custody::new(65536);
        let mut decoder = Decoder::new(custody.clone(), 65536);
        let mut encoder = Encoder::new(custody);
        let parts=decoder.decode(&event(json!({"candidates":[{"content":{"parts":[{"text":"original"}]},"finishReason":"STOP"}]})))?;
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, StreamPart::TextDelta { .. }))
        );
        assert!(
            encoder
                .encode(&StreamPart::TextDelta {
                    text: "rewritten".into()
                })
                .is_err()
        );
        assert!(encoder.finish().is_err());
        let custody = Custody::new(65536);
        let mut decoder = Decoder::new(custody.clone(), 65536);
        let mut encoder = Encoder::new(custody);
        assert!(
            observe(
                &mut decoder,
                &mut encoder,
                &event(json!({"candidates":[{"finishReason":"STOP"}]}))
            )?
            .is_empty()
        );
        assert!(
            decoder
                .decode(&event(
                    json!({"candidates":[{"content":{"parts":[{"text":"late"}]}}]})
                ))
                .is_err()
        );
        assert!(decoder.finish().is_err());
        assert!(encoder.finish().is_err());
        Ok(())
    }
}
