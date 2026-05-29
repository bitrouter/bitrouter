//! Streaming primitives for the `language_model` protocol: `StreamInterest`,
//! `StreamAction` / `StreamOutcome`, the `SseFrame` type, `SseKeepaliveStream`,
//! and the `StreamProcessor` that drives the StreamHook stage.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use pin_project_lite::pin_project;
use tokio::time::{Instant, Sleep};

use crate::error::{BitrouterError, Result};
use crate::language_model::context::StreamContext;
use crate::language_model::hooks::{ObserveHook, StreamHook};
use crate::language_model::types::{StreamPart, Usage};

/// A bitset of the `StreamPart` kinds a hook cares about. Lets the pipeline skip
/// hooks on parts they declared no interest in (keeps the per-token hot path
/// proportional to declared interest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamInterest(u8);

impl StreamInterest {
    const TEXT_DELTA: u8 = 1 << 0;
    const REASONING_DELTA: u8 = 1 << 1;
    const TOOL_CALL_DELTA: u8 = 1 << 2;
    const USAGE: u8 = 1 << 3;
    const FINISH: u8 = 1 << 4;
    const RESPONSE_STARTED: u8 = 1 << 5;

    /// Interested in nothing.
    pub const fn none() -> Self {
        Self(0)
    }

    /// Interested in every part kind.
    pub const fn all() -> Self {
        Self(
            Self::TEXT_DELTA
                | Self::REASONING_DELTA
                | Self::TOOL_CALL_DELTA
                | Self::USAGE
                | Self::FINISH
                | Self::RESPONSE_STARTED,
        )
    }

    /// Add interest in `TextDelta`.
    pub const fn with_text_delta(self) -> Self {
        Self(self.0 | Self::TEXT_DELTA)
    }

    /// Add interest in `ReasoningDelta`.
    pub const fn with_reasoning_delta(self) -> Self {
        Self(self.0 | Self::REASONING_DELTA)
    }

    /// Add interest in `ToolCallDelta`.
    pub const fn with_tool_call_delta(self) -> Self {
        Self(self.0 | Self::TOOL_CALL_DELTA)
    }

    /// Add interest in `Usage`.
    pub const fn with_usage(self) -> Self {
        Self(self.0 | Self::USAGE)
    }

    /// Add interest in `Finish`.
    pub const fn with_finish(self) -> Self {
        Self(self.0 | Self::FINISH)
    }

    /// Add interest in `ResponseStarted`.
    pub const fn with_response_started(self) -> Self {
        Self(self.0 | Self::RESPONSE_STARTED)
    }

    /// Whether this interest set matches `part`.
    pub fn matches(&self, part: &StreamPart) -> bool {
        let bit = match part {
            StreamPart::TextDelta { .. } => Self::TEXT_DELTA,
            StreamPart::ReasoningDelta { .. } => Self::REASONING_DELTA,
            StreamPart::ToolCallDelta { .. } => Self::TOOL_CALL_DELTA,
            StreamPart::Usage { .. } => Self::USAGE,
            StreamPart::ResponseStarted { .. } => Self::RESPONSE_STARTED,
            // `ResponseCompleted` is a terminal part — a hook interested in
            // `Finish` is, by construction, also interested in it.
            StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. } => Self::FINISH,
        };
        self.0 & bit != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

/// What a `StreamHook` decides to do with a part.
#[derive(Debug)]
pub enum StreamAction {
    /// Emit the part unchanged.
    Pass,
    /// Replace the part with zero or more parts (rewrite / inject / split).
    Replace(Vec<StreamPart>),
    /// Swallow the part — do not emit it.
    Drop,
    /// Abort the stream. The pipeline appends a terminal error frame in the
    /// outbound protocol's format, stops emitting, and fires `on_stream_end`
    /// with `StreamOutcome::Aborted`.
    Abort(BitrouterError),
}

/// How a stream terminated. `on_stream_end` is called for every variant.
#[derive(Debug, Clone)]
pub enum StreamOutcome {
    /// Upstream sent a clean `Finish`.
    Completed,
    /// The client disconnected before completion.
    ClientDisconnected,
    /// A `StreamHook` returned `Abort`.
    Aborted(BitrouterError),
    /// Upstream errored mid-stream.
    UpstreamError(BitrouterError),
}

/// An outbound Server-Sent-Events frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrame {
    /// A data event, optionally named.
    Event {
        /// The `event:` field, if any.
        event: Option<String>,
        /// The `data:` payload (already serialized).
        data: String,
    },
    /// An SSE comment (`:text`). Used for keepalives — every supported protocol
    /// ignores comments.
    Comment(String),
}

impl SseFrame {
    /// Render the frame to its on-wire byte form.
    pub fn to_wire(&self) -> String {
        match self {
            SseFrame::Event { event, data } => match event {
                Some(name) => format!("event: {name}\ndata: {data}\n\n"),
                None => format!("data: {data}\n\n"),
            },
            SseFrame::Comment(text) => format!(":{text}\n\n"),
        }
    }
}

pin_project! {
    /// Wraps any `Stream<Item = SseFrame>` and injects a keepalive comment frame
    /// whenever the inner stream is idle longer than `interval`. Fixes v0 #422
    /// (slow generations dropping the connection after 5 minutes of silence).
    pub struct SseKeepaliveStream<S> {
        #[pin]
        inner: S,
        interval: Duration,
        #[pin]
        timer: Sleep,
        done: bool,
    }
}

impl<S> SseKeepaliveStream<S> {
    /// Wrap `inner`, emitting a `:keepalive` comment after each `interval` of
    /// inner-stream silence.
    pub fn new(inner: S, interval: Duration) -> Self {
        Self {
            inner,
            interval,
            timer: tokio::time::sleep(interval),
            done: false,
        }
    }
}

impl<S> Stream for SseKeepaliveStream<S>
where
    S: Stream<Item = SseFrame>,
{
    type Item = SseFrame;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(frame)) => {
                this.timer.as_mut().reset(Instant::now() + *this.interval);
                Poll::Ready(Some(frame))
            }
            Poll::Ready(None) => {
                *this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => match this.timer.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.timer.as_mut().reset(Instant::now() + *this.interval);
                    Poll::Ready(Some(SseFrame::Comment("keepalive".to_string())))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Accumulates token usage seen across a stream. The last `Usage` part wins
/// (providers send a running or final total, not deltas).
///
/// Also tracks the total *character* count of text and reasoning deltas
/// observed so that — when the consumer hangs up before the upstream `Usage`
/// frame arrives — we can synthesise an estimated output-token count and bill
/// for the work the upstream already did. Without this, a hostile client
/// could drain a long generation, disconnect just before the terminal Usage
/// chunk, and pay nothing. See
/// [`UsageAccumulator::estimated_output_tokens`] and the
/// disconnect branch inside [`StreamProcessor::finish`].
///
/// Heuristic for the estimate: ~4 chars per token (the OpenAI "rule of thumb"
/// for English) — wrong for code / non-Latin scripts but bounded, monotonic
/// in delta length, and far closer to the true cost than `0`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageAccumulator {
    usage: Usage,
    seen: bool,
    /// Total `char` count of `TextDelta` + `ReasoningDelta` text observed.
    delta_chars: u64,
}

impl UsageAccumulator {
    /// Chars-per-token estimate for the disconnect-time billing heuristic.
    /// Conservative-ish: too small bills extra, too large bills less.
    const CHARS_PER_TOKEN_ESTIMATE: u64 = 4;

    /// A fresh, empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a part; updates the running usage from a `Usage` part or from a
    /// `ResponseCompleted` part that carries usage. Text / reasoning deltas
    /// are counted (chars) into the disconnect-time estimate.
    pub fn observe(&mut self, part: &StreamPart) {
        match part {
            StreamPart::Usage { usage }
            | StreamPart::ResponseCompleted {
                usage: Some(usage), ..
            } => {
                self.usage = *usage;
                self.seen = true;
            }
            StreamPart::TextDelta { text } | StreamPart::ReasoningDelta { text } => {
                self.delta_chars = self.delta_chars.saturating_add(text.chars().count() as u64);
            }
            _ => {}
        }
    }

    /// The accumulated usage, if any `Usage` part was seen.
    pub fn finalized(&self) -> Option<Usage> {
        self.seen.then_some(self.usage)
    }

    /// Estimated output-token count from accumulated delta text, for the
    /// disconnect-billing fallback. Returns `0` when no text deltas were
    /// observed.
    pub fn estimated_output_tokens(&self) -> u64 {
        self.delta_chars.div_ceil(Self::CHARS_PER_TOKEN_ESTIMATE)
    }
}

/// Drives the StreamHook stage: applies the hook chain to each part, fans parts
/// out to `ObserveHook`s, and guarantees `on_stream_end` fires exactly once for
/// every hook regardless of how the stream terminated.
///
/// Kept as an explicit driver (rather than a `Stream` impl) so the StreamHook
/// stage is unit-testable in isolation.
pub struct StreamProcessor {
    hooks: Vec<Arc<dyn StreamHook>>,
    observe: Vec<Arc<dyn ObserveHook>>,
    ctx: StreamContext,
    ended: bool,
}

impl StreamProcessor {
    /// Build a processor over the given hooks, observers and stream context.
    pub fn new(
        hooks: Vec<Arc<dyn StreamHook>>,
        observe: Vec<Arc<dyn ObserveHook>>,
        ctx: StreamContext,
    ) -> Self {
        Self {
            hooks,
            observe,
            ctx,
            ended: false,
        }
    }

    /// Shared access to the in-flight stream context.
    pub fn context(&self) -> &StreamContext {
        &self.ctx
    }

    /// Run one part through the hook chain. Returns the parts to emit
    /// downstream, or an `Abort` error if a hook aborted the stream.
    ///
    /// Each hook in registration order sees the (possibly rewritten) output of
    /// the previous hook. The accumulator always observes the *original*
    /// upstream part so usage is never lost to a rewrite.
    pub async fn process_part(&mut self, part: StreamPart) -> Result<Vec<StreamPart>> {
        self.ctx.accumulated_usage.observe(&part);
        self.ctx.parts_emitted += 1;

        let mut current = vec![part];
        for hook in &self.hooks {
            let interest = hook.interest();
            let mut next = Vec::with_capacity(current.len());
            for p in current {
                if !interest.matches(&p) {
                    next.push(p);
                    continue;
                }
                // Clone for the hook so `StreamAction::Pass` can re-emit the
                // original verbatim — the hook receives the part by value per
                // the trait signature, but the processor keeps the identity.
                let original = p.clone();
                match hook.on_part(&mut self.ctx, p).await? {
                    StreamAction::Pass => next.push(original),
                    StreamAction::Replace(parts) => next.extend(parts),
                    StreamAction::Drop => {}
                    StreamAction::Abort(err) => return Err(err),
                }
            }
            current = next;
        }

        // Fan out to observers (read-only, errors swallowed).
        for p in &current {
            for obs in &self.observe {
                if obs.stream_interest().matches(p) {
                    obs.on_stream_part(&self.ctx, p).await;
                }
            }
        }
        Ok(current)
    }

    /// Terminate the stream: fire `on_stream_end` on every hook exactly once.
    /// Idempotent — repeated calls are no-ops. Hook errors are swallowed (the
    /// stream is already over) but logged.
    ///
    /// Settlement billing rules at the end of a stream:
    /// - If an authoritative `Usage` part was observed at any point, that
    ///   wins — covers both clean termination and disconnect *after* the
    ///   usage chunk.
    /// - If the stream ended via [`StreamOutcome::ClientDisconnected`] before
    ///   any `Usage` was seen but delta text *was* observed, synthesise a
    ///   usage with `completion_tokens` estimated from the text length so
    ///   the request still bills. Without this, a client could drain a long
    ///   generation, hang up just before the trailing usage frame, and pay
    ///   $0. Prompt tokens stay at `0` in the estimate (the prompt isn't
    ///   plumbed through `StreamContext` — known gap, mirrors the v0 fix).
    /// - Otherwise (clean error with no usage, or disconnect with no
    ///   deltas), leave `final_usage` empty; settlement records the request
    ///   as un-billable.
    pub async fn finish(&mut self, outcome: StreamOutcome) -> &StreamContext {
        if self.ended {
            return &self.ctx;
        }
        self.ended = true;
        for hook in &self.hooks {
            if let Err(e) = hook.on_stream_end(&mut self.ctx, &outcome).await {
                tracing::warn!(error = %e, "StreamHook::on_stream_end failed");
            }
        }
        if let Some(usage) = self.ctx.accumulated_usage.finalized() {
            self.ctx.final_usage = Some(usage);
        } else if matches!(outcome, StreamOutcome::ClientDisconnected) {
            let estimated = self.ctx.accumulated_usage.estimated_output_tokens();
            if estimated > 0 {
                tracing::warn!(
                    request_id = %self.ctx.request_id,
                    estimated_output_tokens = estimated,
                    "client disconnected mid-stream before upstream usage frame; billing estimated output"
                );
                self.ctx.final_usage = Some(Usage {
                    completion_tokens: estimated,
                    ..Default::default()
                });
            }
        }
        &self.ctx
    }

    /// Consume the processor, yielding the final stream context.
    pub fn into_context(self) -> StreamContext {
        self.ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::FinishReason;
    use futures::StreamExt;

    #[test]
    fn interest_matches_only_declared_kinds() {
        let i = StreamInterest::none().with_usage().with_finish();
        assert!(i.matches(&StreamPart::Usage {
            usage: Usage::default()
        }));
        assert!(i.matches(&StreamPart::Finish {
            reason: FinishReason::Stop
        }));
        assert!(!i.matches(&StreamPart::TextDelta {
            text: "x".to_string()
        }));
        assert!(StreamInterest::all().matches(&StreamPart::TextDelta {
            text: "x".to_string()
        }));
        assert!(!StreamInterest::none().matches(&StreamPart::TextDelta {
            text: "x".to_string()
        }));
    }

    #[test]
    fn sse_frame_wire_format() {
        assert_eq!(
            SseFrame::Event {
                event: None,
                data: "{}".to_string()
            }
            .to_wire(),
            "data: {}\n\n"
        );
        assert_eq!(
            SseFrame::Event {
                event: Some("message".to_string()),
                data: "x".to_string()
            }
            .to_wire(),
            "event: message\ndata: x\n\n"
        );
        assert_eq!(
            SseFrame::Comment("keepalive".to_string()).to_wire(),
            ":keepalive\n\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_injects_comment_on_idle() {
        // A stream that yields one frame, then stays pending forever.
        let slow = futures::stream::once(async {
            SseFrame::Event {
                event: None,
                data: "first".to_string(),
            }
        })
        .chain(futures::stream::pending());

        let ka = SseKeepaliveStream::new(slow, Duration::from_secs(30));
        let mut ka = std::pin::pin!(ka);

        // The real data frame comes through immediately.
        assert_eq!(
            ka.next().await,
            Some(SseFrame::Event {
                event: None,
                data: "first".to_string()
            })
        );
        // After the idle interval, a keepalive comment is injected.
        let next = ka.next().await;
        assert_eq!(next, Some(SseFrame::Comment("keepalive".to_string())));
    }

    #[tokio::test]
    async fn keepalive_passes_through_when_not_idle() {
        let fast = futures::stream::iter(vec![
            SseFrame::Event {
                event: None,
                data: "a".to_string(),
            },
            SseFrame::Event {
                event: None,
                data: "b".to_string(),
            },
        ]);
        let ka = SseKeepaliveStream::new(fast, Duration::from_secs(30));
        let frames: Vec<_> = ka.collect().await;
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| matches!(f, SseFrame::Event { .. })));
    }
}
