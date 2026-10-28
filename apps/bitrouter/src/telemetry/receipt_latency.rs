//! Private, bounded observation of existing receipt custody. Never authority.
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, ReceiptState},
    identity::CorrelationId,
};
use serde::Deserialize;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub(crate) mod capacity;

pub(super) const UPDATE_LIMIT: u64 = 100_000_000;
pub(super) const MAX_NS: u64 = 120_000_000_000;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub enabled: bool,
    correlation_id: Option<CorrelationId>,
}
impl Config {
    pub(crate) fn read(config: &crate::config::Config) -> anyhow::Result<Self> {
        let value = config
            .plugins
            .get("bitrouter-telemetry")
            .and_then(|plugin| plugin.get("receipt_latency"));
        let parsed: Self = match value {
            Some(value) => serde_json::from_value(value.clone())?,
            None => Self::default(),
        };
        if let Some(id) = &parsed.correlation_id {
            anyhow::ensure!(
                id.as_str().len() <= 128,
                "receipt latency correlation exceeds 128 bytes"
            );
        }
        if parsed.enabled {
            anyhow::ensure!(
                super::OTEL_ENABLED,
                "receipt latency requires an OTLP transport"
            );
            anyhow::ensure!(
                config.tenancy.enabled,
                "receipt latency requires local tenancy"
            );
            let plugin = config.plugins.get("bitrouter-telemetry");
            anyhow::ensure!(
                plugin.and_then(|p| p.get("otel")).is_some(),
                "receipt latency requires explicit OTLP configuration"
            );
            let otel: super::config::OtelConfig =
                serde_json::from_value(plugin.and_then(|p| p.get("otel")).cloned().ok_or_else(
                    || anyhow::anyhow!("receipt latency OTLP configuration missing"),
                )?)?;
            anyhow::ensure!(
                otel.metrics.enabled,
                "receipt latency requires enabled OTLP metrics"
            );
            anyhow::ensure!(
                plugin.and_then(|p| p.get("telemetry")).is_none(),
                "receipt latency requires the explicit OTLP configuration path"
            );
        }
        Ok(parsed)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    FirstQueue,
    Prepare,
    Transaction,
    SqlCommit,
}
impl Phase {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::FirstQueue => "first_queue",
            Self::Prepare => "prepare",
            Self::Transaction => "transaction",
            Self::SqlCommit => "sql_commit",
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) enum ResultKind {
    Observed,
    Acknowledged,
    Failed,
    Unconfirmed,
}
impl ResultKind {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Acknowledged => "acknowledged",
            Self::Failed => "failed",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Event {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    Enabled,
    Admitted,
    FirstQueued,
    Retry,
    QueueFailure,
    Predeadline,
    SqlAck,
    SqlUnconfirmed,
    RollbackAck,
    PublicationFailure,
    CallerWaitFailure,
    Unclassified,
    ClockInvalid,
    RangeOverflow,
    SealedClean,
    SealedFailed,
}
impl Event {
    pub(super) fn label(self) -> &'static str {
        match self {
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            Self::Enabled => "enabled",
            Self::Admitted => "admitted",
            Self::FirstQueued => "first_queued",
            Self::Retry => "retry",
            Self::QueueFailure => "queue_failure",
            Self::Predeadline => "predeadline",
            Self::SqlAck => "sql_ack",
            Self::SqlUnconfirmed => "sql_unconfirmed",
            Self::RollbackAck => "rollback_ack",
            Self::PublicationFailure => "publication_failure",
            Self::CallerWaitFailure => "caller_wait_failure",
            Self::Unclassified => "unclassified",
            Self::ClockInvalid => "clock_invalid",
            Self::RangeOverflow => "range_overflow",
            Self::SealedClean => "sealed_clean",
            Self::SealedFailed => "sealed_failed",
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum Class {
    CompleteCompleted,
    TerminalOther,
    EvidenceUnknown,
}
impl Class {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::CompleteCompleted => "complete_completed",
            Self::TerminalOther => "terminal_other",
            Self::EvidenceUnknown => "evidence_unknown",
        }
    }
    fn of(receipt: &ExecutionReceipt) -> Self {
        if receipt.state == ReceiptState::Unknown || receipt.outcome == Outcome::Unknown {
            Self::EvidenceUnknown
        } else if receipt.state == ReceiptState::Committed
            && receipt.outcome == Outcome::Completed
            && receipt.usage.complete
        {
            Self::CompleteCompleted
        } else {
            Self::TerminalOther
        }
    }
}

// Implemented only by the instruments registered on the existing meter provider.
// Static enums are the entire attribute vocabulary; no identity/payload is retained.
pub(super) trait Recorder: Send + Sync {
    fn phase(&self, value: u64, phase: Phase, result: ResultKind);
    fn durable(&self, value: u64, class: Class);
    fn event(&self, event: &'static str, value: u64);
}
struct State {
    recorder: Arc<dyn Recorder>,
    correlation: Option<CorrelationId>,
    updates: AtomicU64,
    rejected: AtomicU64,
    rejection_saturated: AtomicBool,
    invalid: AtomicBool,
    admitted: AtomicU64,
    queued: AtomicU64,
    durable: AtomicU64,
    seal: OnceLock<bool>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    capacity: Arc<capacity::Observation>,
}
#[derive(Clone, Default)]
pub(crate) struct Sink(Option<Arc<State>>);
impl Sink {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(super) fn new(
        config: &Config,
        recorder: Arc<dyn Recorder>,
        capacity: Arc<capacity::Observation>,
    ) -> Self {
        let sink = Self(Some(Arc::new(State {
            recorder,
            correlation: config.correlation_id.clone(),
            updates: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            rejection_saturated: AtomicBool::new(false),
            invalid: AtomicBool::new(false),
            admitted: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            durable: AtomicU64::new(0),
            seal: OnceLock::new(),
            capacity,
        })));
        sink.event(Event::Enabled);
        sink
    }
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn bind_capacity(
        &self,
        initial: capacity::Snapshot,
        read: impl Fn() -> Result<capacity::Snapshot, capacity::ReadError> + Send + Sync + 'static,
    ) -> bool {
        self.0
            .as_ref()
            .is_some_and(|s| s.capacity.bind(initial, read))
    }
    #[cfg(all(test, feature = "otel-http"))]
    pub(crate) fn capacity_for_test(&self) -> Option<Arc<capacity::Observation>> {
        self.0.as_ref().map(|state| state.capacity.clone())
    }
    pub(crate) fn enabled(&self) -> bool {
        self.0.is_some()
    }
    pub(crate) fn timing(&self, correlation: &CorrelationId) -> Timing {
        Timing(
            self.0
                .as_ref()
                .filter(|s| s.correlation.as_ref().is_none_or(|id| id == correlation))
                .map(|_| OwnerTiming {
                    sink: self.clone(),
                    ready: OnceLock::new(),
                    first_dequeue: AtomicBool::new(false),
                    admitted: AtomicBool::new(false),
                    durable: AtomicBool::new(false),
                    entered: AtomicBool::new(false),
                }),
        )
    }
    fn reserve(&self) -> Option<&State> {
        // Explicit CAS keeps the same bound/order on supported Rust toolchains.
        fn increment(counter: &AtomicU64) -> bool {
            let mut current = counter.load(Ordering::Relaxed);
            while current < UPDATE_LIMIT {
                match counter.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(actual) => current = actual,
                }
            }
            false
        }
        let state = self.0.as_deref()?;
        if state.seal.get().is_none() && increment(&state.updates) {
            return Some(state);
        }
        state.invalid.store(true, Ordering::Relaxed);
        if !increment(&state.rejected) {
            state.rejection_saturated.store(true, Ordering::Relaxed);
        }
        None
    }
    fn invalid(&self) {
        if let Some(s) = &self.0 {
            s.invalid.store(true, Ordering::Relaxed);
        }
    }
    pub(crate) fn event(&self, event: Event) {
        if let Some(s) = self.reserve() {
            s.recorder.event(event.label(), 1);
        }
    }
    fn duration(&self, start: Instant, end: Instant, emit: impl FnOnce(&dyn Recorder, u64)) {
        let Some(duration) = end.checked_duration_since(start) else {
            self.invalid();
            self.event(Event::ClockInvalid);
            return;
        };
        let Some(ns) = bounded_ns(duration) else {
            self.invalid();
            self.event(Event::RangeOverflow);
            return;
        };
        if let Some(s) = self.reserve() {
            emit(s.recorder.as_ref(), ns);
        }
    }
    /// Call only after the existing producer/actor shutdown. This is a numeric
    /// completeness seal, not an export ACK or a receipt/output ACK.
    pub(crate) fn seal(&self, producers_clean: bool) -> bool {
        let Some(s) = &self.0 else {
            return true;
        };
        *s.seal.get_or_init(|| {
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            let producers_clean = s.capacity.seal(producers_clean);
            let admitted = s.admitted.load(Ordering::Relaxed);
            let clean = producers_clean
                && admitted > 0
                && admitted == s.queued.load(Ordering::Relaxed)
                && admitted == s.durable.load(Ordering::Relaxed)
                && !s.invalid.load(Ordering::Relaxed);
            // Reserved final series bypass the exhausted update budget exactly once.
            // Each final counter is <= UPDATE_LIMIT, never u64::MAX.
            s.recorder
                .event("sample_limit", s.rejected.load(Ordering::Relaxed));
            s.recorder.event(
                "sample_limit_saturated",
                u64::from(s.rejection_saturated.load(Ordering::Relaxed)),
            );
            s.recorder.event(
                if clean {
                    Event::SealedClean
                } else {
                    Event::SealedFailed
                }
                .label(),
                1,
            );
            clean
        })
    }
}
fn bounded_ns(duration: Duration) -> Option<u64> {
    let ns = u64::try_from(duration.as_nanos()).ok()?;
    (ns <= MAX_NS).then_some(ns)
}

#[derive(Default)]
pub(crate) struct Timing(Option<OwnerTiming>);
struct OwnerTiming {
    sink: Sink,
    ready: OnceLock<Instant>,
    first_dequeue: AtomicBool,
    admitted: AtomicBool,
    durable: AtomicBool,
    entered: AtomicBool,
}
impl Timing {
    pub(crate) fn admitted(&self) {
        if let Some(t) = &self.0
            && !t.admitted.swap(true, Ordering::Relaxed)
            && let Some(s) = t.sink.reserve()
        {
            s.admitted.fetch_add(1, Ordering::Relaxed);
            s.recorder.event(Event::Admitted.label(), 1);
        }
    }
    /// Called inside the existing Open -> Queued lock; only clock/state here.
    pub(crate) fn ready(&self) {
        if let Some(t) = &self.0 {
            t.ready.get_or_init(Instant::now);
        }
    }
    /// Emit outside the phase lock, before enqueue. Retries never call ready().
    pub(crate) fn queued(&self) {
        if let Some(t) = &self.0
            && let Some(s) = t.sink.reserve()
        {
            s.queued.fetch_add(1, Ordering::Relaxed);
            s.recorder.event(Event::FirstQueued.label(), 1);
        }
    }
    pub(crate) fn dequeue(&self) {
        if let Some(t) = &self.0 {
            t.entered.store(false, Ordering::Relaxed);
            if !t.first_dequeue.swap(true, Ordering::Relaxed) {
                if let Some(start) = t.ready.get() {
                    t.sink.duration(*start, Instant::now(), |r, ns| {
                        r.phase(ns, Phase::FirstQueue, ResultKind::Observed)
                    });
                } else {
                    t.sink.invalid();
                    t.sink.event(Event::Unclassified);
                }
            }
        }
    }
    pub(crate) fn entered(&self) {
        if let Some(t) = &self.0 {
            t.entered.store(true, Ordering::Relaxed);
        }
    }
    pub(crate) fn returned(&self, failed: bool) {
        if failed
            && let Some(t) = &self.0
            && !t.entered.load(Ordering::Relaxed)
        {
            t.sink.event(Event::Predeadline);
        }
    }
    pub(crate) fn event(&self, event: Event) {
        if let Some(t) = &self.0 {
            t.sink.event(event);
        }
    }
    pub(crate) fn span(&self, phase: Phase) -> Span {
        Span(
            self.0
                .as_ref()
                .map(|t| (t.sink.clone(), Instant::now(), phase)),
        )
    }
    /// Capture only after the real Durable transition, before unlocking/notify.
    pub(crate) fn publication(&self, receipt: &ExecutionReceipt) -> Publication {
        Publication(self.0.as_ref().and_then(|t| {
            if t.durable.swap(true, Ordering::Relaxed) {
                return None;
            }
            let end = Instant::now();
            Some((
                t.sink.clone(),
                t.ready.get().copied(),
                end,
                Class::of(receipt),
            ))
        }))
    }
}
#[derive(Default)]
pub(crate) struct Span(Option<(Sink, Instant, Phase)>);
impl Span {
    pub(crate) fn stop(mut self, result: ResultKind) -> FinishedSpan {
        FinishedSpan(
            self.0
                .take()
                .map(|(sink, start, phase)| (sink, start, Instant::now(), phase, result)),
        )
    }
    pub(crate) fn finish(self, result: ResultKind) {
        self.stop(result).record();
    }
}
pub(crate) struct FinishedSpan(Option<(Sink, Instant, Instant, Phase, ResultKind)>);
impl FinishedSpan {
    pub(crate) fn record(self) {
        if let Some((sink, start, end, phase, result)) = self.0 {
            sink.duration(start, end, |r, ns| r.phase(ns, phase, result));
            if matches!(phase, Phase::SqlCommit) {
                sink.event(if matches!(result, ResultKind::Acknowledged) {
                    Event::SqlAck
                } else {
                    Event::SqlUnconfirmed
                });
            }
            if matches!(result, ResultKind::Unconfirmed) {
                sink.invalid();
            }
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some((sink, start, phase)) = self.0.take() {
            sink.invalid();
            sink.duration(start, Instant::now(), |r, ns| {
                r.phase(ns, phase, ResultKind::Unconfirmed)
            });
            sink.event(if matches!(phase, Phase::SqlCommit) {
                Event::SqlUnconfirmed
            } else {
                Event::Unclassified
            });
        }
    }
}
pub(crate) struct Publication(Option<(Sink, Option<Instant>, Instant, Class)>);
impl Publication {
    pub(crate) fn record(self) {
        if let Some((sink, start, end, class)) = self.0 {
            if matches!(class, Class::EvidenceUnknown) {
                sink.invalid();
            }
            if let Some(start) = start {
                sink.duration(start, end, |r, ns| r.durable(ns, class));
                if let Some(s) = sink.reserve() {
                    s.durable.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                sink.invalid();
                sink.event(Event::Unclassified);
            }
        }
    }
}
