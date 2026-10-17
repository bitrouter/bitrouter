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

struct ActiveWork {
    work: NativeProviderWork,
    started: Option<Instant>,
}

#[derive(Default)]
struct Observation {
    active: Option<ActiveWork>,
    pending: Option<NativeProviderWorkReport>,
}

/// Shared with the HTTP executor through a request-local context extension.
pub(crate) struct NativeWorkRuntime {
    control: Arc<dyn NativeExecutionControl>,
    attempt: Mutex<Option<Attempt>>,
    observation: tokio::sync::Mutex<Observation>,
    gate_nanos: AtomicU64,
}

impl NativeWorkRuntime {
    pub(crate) fn new(control: Arc<dyn NativeExecutionControl>) -> Self {
        Self {
            control,
            attempt: Mutex::new(None),
            observation: tokio::sync::Mutex::new(Observation::default()),
            gate_nanos: AtomicU64::new(0),
        }
    }

    pub(crate) async fn flush(&self) {
        self.flush_report(true).await;
    }

    async fn flush_report(&self, finish_abandoned: bool) {
        let pending = {
            let mut observation = self.observation.lock().await;
            observation.pending.take().or_else(|| {
                if !finish_abandoned {
                    return None;
                }
                observation
                    .active
                    .take()
                    .map(|active| NativeProviderWorkReport {
                        work: active.work,
                        elapsed_ms: active.started.map_or(0, super::timing::elapsed_millis),
                        http_status: None,
                        error_code: Some(BitrouterError::Cancelled.error_code().into()),
                    })
            })
        };
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
    runtime.flush_report(false).await;
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
    {
        let mut observation = runtime.observation.lock().await;
        if observation.active.is_some() {
            return Err(unavailable());
        }
        // Retain identity before the durable callback: cancellation can win
        // after its intent committed but before its acknowledgement returned.
        observation.active = Some(ActiveWork {
            work: work.clone(),
            started: None,
        });
    }
    let gate = Instant::now();
    let allowed = runtime.control.before_provider_work(&work).await;
    runtime.add_gate_time(gate.elapsed());
    if let Err(error) = allowed {
        let mut observation = runtime.observation.lock().await;
        observation.active = None;
        // A rejected admission may have no durable intent at all. Do not
        // submit an outcome for it. A dropped callback instead leaves active
        // identity for the enclosing attempt's cancellation settlement.
        return Err(error);
    }
    let started = Instant::now();
    runtime
        .observation
        .lock()
        .await
        .active
        .as_mut()
        .ok_or_else(unavailable)?
        .started = Some(started);
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
    let mut observation = runtime.observation.lock().await;
    observation.active = None;
    observation.pending = Some(report);
    result
}

#[cfg(test)]
mod tests {
    use super::super::native::{NativeAttemptReport, NativePlan, NativePlanAdmission};
    use super::super::types::PipelineRequest;
    use super::*;
    use crate::caller::CallerContext;
    use bitrouter_ai::types::Prompt;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct Control {
        stop_in_admission: bool,
        reject_admission: bool,
        entered: tokio::sync::Notify,
        reports: tokio::sync::Mutex<Vec<NativeProviderWorkReport>>,
    }

    #[async_trait::async_trait]
    impl NativeExecutionControl for Control {
        async fn plan(&self, _: NativePlan) -> Result<NativePlanAdmission> {
            Ok(NativePlanAdmission {
                route_indices: vec![0],
            })
        }
        async fn before_attempt(&self, _: &str, _: u32) -> Result<()> {
            Ok(())
        }
        async fn after_attempt(&self, _: NativeAttemptReport) {}
        async fn before_provider_work(&self, _: &NativeProviderWork) -> Result<()> {
            if self.reject_admission {
                return Err(unavailable());
            }
            if self.stop_in_admission {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn after_provider_work(&self, report: NativeProviderWorkReport) {
            self.reports.lock().await.push(report);
        }
    }

    fn context(control: Arc<Control>) -> Result<(PipelineContext, Arc<NativeWorkRuntime>)> {
        let prompt = Prompt {
            model: "fixture".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut ctx = PipelineContext::new(PipelineRequest::new(
            "fixture",
            CallerContext::local(),
            prompt,
        ));
        let runtime = Arc::new(NativeWorkRuntime::new(control));
        runtime.begin_attempt(0)?;
        ctx.insert_extension(runtime.clone());
        Ok((ctx, runtime))
    }

    async fn cancelled(admission: bool) -> Result<()> {
        let control = Arc::new(Control {
            stop_in_admission: admission,
            ..Default::default()
        });
        let (ctx, runtime) = context(control.clone())?;
        let dispatched = AtomicBool::new(false);
        let mut operation = Box::pin(observe(
            &ctx,
            NativeProviderWorkKind::HttpDispatch,
            async {
                dispatched.store(true, Ordering::SeqCst);
                control.entered.notify_one();
                std::future::pending::<Result<()>>().await
            },
            |_| None,
        ));
        tokio::select! {
            _ = control.entered.notified() => {},
            _ = &mut operation => return Err(unavailable()),
        }
        drop(operation);
        runtime.flush().await;
        runtime.flush().await;
        let reports = control.reports.lock().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].work.request_id, ctx.request_id());
        assert_eq!(reports[0].work.attempt_index, 0);
        assert_eq!(reports[0].work.work_index, 0);
        assert_eq!(reports[0].work.kind, NativeProviderWorkKind::HttpDispatch);
        assert_eq!(reports[0].http_status, None);
        assert_eq!(reports[0].error_code.as_deref(), Some("request_cancelled"));
        assert_eq!(dispatched.load(Ordering::SeqCst), !admission);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_http_future_records_one_unknown_outcome() -> Result<()> {
        cancelled(false).await
    }

    #[tokio::test]
    async fn cancelled_durable_admission_preserves_identity_without_dispatch() -> Result<()> {
        cancelled(true).await
    }

    #[tokio::test]
    async fn accepted_headers_keep_their_original_observation_after_flush() -> Result<()> {
        let control = Arc::new(Control::default());
        let (ctx, runtime) = context(control.clone())?;
        let status = observe(
            &ctx,
            NativeProviderWorkKind::HttpDispatch,
            async { Ok(201_u16) },
            |status| Some(*status),
        )
        .await?;
        assert_eq!(status, 201);
        runtime.flush().await;
        runtime.flush().await;
        let reports = control.reports.lock().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].http_status, Some(201));
        assert_eq!(reports[0].error_code, None);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_admission_does_not_report_an_uncommitted_intent() -> Result<()> {
        let control = Arc::new(Control {
            reject_admission: true,
            ..Default::default()
        });
        let (ctx, runtime) = context(control.clone())?;
        let dispatched = AtomicBool::new(false);
        let result = observe(
            &ctx,
            NativeProviderWorkKind::HttpDispatch,
            async {
                dispatched.store(true, Ordering::SeqCst);
                Ok(())
            },
            |_| None,
        )
        .await;
        assert!(result.is_err());
        runtime.flush().await;
        assert!(!dispatched.load(Ordering::SeqCst));
        assert!(control.reports.lock().await.is_empty());
        Ok(())
    }
}
