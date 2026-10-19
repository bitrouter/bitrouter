//! Durable observations of provider integration work within a routed attempt.
//! No credentials, request bodies or upstream diagnostic text cross this boundary.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::context::PipelineContext;
use super::native::NativeExecutionControl;
use crate::error::{BitrouterError, Result};

/// An execution phase, not a separate invoice or successful generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeProviderWorkKind {
    /// An installed authentication extension prepares the request body.
    AuthenticationPreparation,
    /// Build and validate one authenticated request.
    Authentication,
    /// Ask the authentication extension to refresh rejected credentials.
    AuthenticationRefresh,
    /// Send one HTTP request through receipt of response headers. Body decoding
    /// remains part of the containing provider attempt, including for streams.
    HttpDispatch,
}

/// Stable identity within one selected provider attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeProviderWork {
    /// Logical SDK request, shared with settlement and all provider fallbacks.
    pub request_id: String,
    /// Position in the immutable route chain.
    pub attempt_index: u32,
    /// Sequential phase identity within this attempt, including retries.
    pub work_index: u32,
    /// Actual phase being authorized.
    pub kind: NativeProviderWorkKind,
}

/// Outcome of integration work; missing monetary usage remains unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeProviderWorkReport {
    /// Identity of the acknowledged intent.
    pub work: NativeProviderWork,
    /// Work time only, excluding durable callbacks.
    pub elapsed_ms: u64,
    /// Received HTTP status, including failures; absent without response headers.
    pub http_status: Option<u16>,
    /// Controlled SDK error category; never diagnostic text or credentials.
    pub error_code: Option<String>,
}

struct Attempt {
    index: u32,
    next_work: u32,
}

/// Shared with the HTTP executor through a request-local context extension.
pub(crate) struct NativeWorkRuntime {
    control: Arc<dyn NativeExecutionControl>,
    attempt: Mutex<Option<Attempt>>,
    pending: tokio::sync::Mutex<Option<NativeProviderWorkReport>>,
    gate_nanos: AtomicU64,
}

impl NativeWorkRuntime {
    pub(crate) fn new(control: Arc<dyn NativeExecutionControl>) -> Self {
        Self {
            control,
            attempt: Mutex::new(None),
            pending: tokio::sync::Mutex::new(None),
            gate_nanos: AtomicU64::new(0),
        }
    }

    pub(crate) async fn flush(&self) {
        let pending = self.pending.lock().await.take();
        if let Some(report) = pending {
            let gate = Instant::now();
            self.control.after_provider_work(report).await;
            self.add_gate_time(gate.elapsed());
        }
    }

    pub(crate) fn begin_attempt(&self, index: u32) -> Result<()> {
        *self.attempt.lock().map_err(|_| unavailable())? = Some(Attempt {
            index,
            next_work: 0,
        });
        self.gate_nanos.store(0, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn work_duration(&self, wall: Duration) -> Duration {
        wall.saturating_sub(self.gate_duration())
    }

    fn gate_duration(&self) -> Duration {
        Duration::from_nanos(self.gate_nanos.load(Ordering::Relaxed))
    }

    fn add_gate_time(&self, elapsed: Duration) {
        let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let mut prior = self.gate_nanos.load(Ordering::Relaxed);
        loop {
            match self.gate_nanos.compare_exchange_weak(
                prior,
                prior.saturating_add(nanos),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => prior = current,
            }
        }
    }
}

pub(crate) fn gate_duration(ctx: &PipelineContext) -> Duration {
    ctx.extension::<NativeWorkRuntime>()
        .map_or(Duration::ZERO, |runtime| runtime.gate_duration())
}

pub(crate) fn elapsed_work_millis(
    ctx: &PipelineContext,
    started: Instant,
    initial_gate: Duration,
) -> u64 {
    super::timing::duration_millis(
        started
            .elapsed()
            .saturating_sub(gate_duration(ctx).saturating_sub(initial_gate)),
    )
}

fn unavailable() -> BitrouterError {
    BitrouterError::internal("provider work observation unavailable")
}

/// Called around actual integration I/O, identically for direct and bridged
/// streaming execution. Ordinary HTTP entry has no durable embedding control.
pub(crate) async fn observe<T>(
    ctx: &PipelineContext,
    kind: NativeProviderWorkKind,
    operation: impl Future<Output = Result<T>>,
    status: impl FnOnce(&T) -> Option<u16>,
) -> Result<T> {
    let Some(runtime) = ctx.extension::<NativeWorkRuntime>() else {
        return operation.await;
    };
    // Drain an accepted HTTP response body before waiting for its observation
    // ACK. Flush before the next integration operation, or at executor return.
    // This lets usage settle even if an ACK is lost after response headers.
    runtime.flush().await;
    let work = {
        let mut state = runtime.attempt.lock().map_err(|_| unavailable())?;
        match state.as_mut() {
            Some(attempt) => {
                let work = NativeProviderWork {
                    request_id: ctx.request_id().into(),
                    attempt_index: attempt.index,
                    work_index: attempt.next_work,
                    kind,
                };
                attempt.next_work = attempt.next_work.checked_add(1).ok_or_else(unavailable)?;
                Some(work)
            }
            None => None,
        }
    };
    let Some(work) = work else {
        // Input counting has its own acknowledged intent and no model attempt.
        return operation.await;
    };
    let _ = super::native_auxiliary::limit(runtime.control.as_ref(), &work.request_id)?;
    let gate = Instant::now();
    let allowed = runtime.control.before_provider_work(&work).await;
    runtime.add_gate_time(gate.elapsed());
    allowed?;
    let started = Instant::now();
    let result = operation.await;
    let report = NativeProviderWorkReport {
        work,
        elapsed_ms: super::timing::elapsed_millis(started),
        http_status: result.as_ref().ok().and_then(status),
        error_code: result
            .as_ref()
            .err()
            .map(|error| error.error_code().to_owned()),
    };
    *runtime.pending.lock().await = Some(report);
    result
}
