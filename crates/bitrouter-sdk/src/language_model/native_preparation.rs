//! Durable admission for configured preparation callbacks. Callback completion
//! and duration are work evidence, never proof of a model call or a zero bill.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::context::PipelineContext;
use super::native::NativeExecutionControl;
use crate::error::{BitrouterError, Result};

/// Configured callback category, without implementation names or private input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePreparationWorkKind {
    /// App-level prompt normalization or selector transformation.
    PromptTransform,
    /// Local ingress/session/continuation normalization.
    PreResolutionHook,
    /// Effective-router preparation before request checks.
    RouterPreparationHook,
    /// Local policy and guardrail checks.
    PreRequestHook,
    /// One configured named-router checker invocation.
    RequestCheck,
    /// Effective model/effort selection for a configured policy.
    ModelSelection,
    /// Selector binding or serving-route lookup.
    RouterLookup,
    /// Validation or modification of the serving chain.
    RouteHook,
}

/// Exact callback identity within one SDK request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePreparationWork {
    /// Shared with the subsequent model plan and request settlement, if reached.
    pub request_id: String,
    /// Actual callback category being admitted.
    pub kind: NativePreparationWorkKind,
    /// App transforms and pipeline preparation each have an ordered sequence.
    /// Pipeline indices restart at zero after all App transforms have finished.
    pub work_index: u32,
}

/// Callback execution evidence independent of any monetary estimate or receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePreparationWorkReport {
    /// Identity of the durably admitted callback.
    pub work: NativePreparationWork,
    /// Callback time only, excluding durable admission and outcome ACKs.
    pub elapsed_ms: u64,
    /// Controlled SDK error category; excludes checker diagnostics and content.
    pub error_code: Option<String>,
}

pub(crate) struct NativePreparationRuntime {
    control: Arc<dyn NativeExecutionControl>,
    next_work: AtomicU32,
    active: AtomicBool,
}

impl NativePreparationRuntime {
    pub(crate) fn new(control: Arc<dyn NativeExecutionControl>) -> Self {
        Self {
            control,
            next_work: AtomicU32::new(0),
            active: AtomicBool::new(true),
        }
    }

    pub(crate) fn finish(&self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn runtime(ctx: &PipelineContext) -> Option<Arc<NativePreparationRuntime>> {
    ctx.extension::<NativePreparationRuntime>()
}

/// App transforms run before a pipeline context exists, but use the same SDK
/// request identity and the same control as subsequent preparation callbacks.
pub(crate) async fn observe<T>(
    control: &dyn NativeExecutionControl,
    work: NativePreparationWork,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    control.before_preparation_work(&work).await?;
    let started = Instant::now();
    let result = operation.await;
    control
        .after_preparation_work(NativePreparationWorkReport {
            work,
            elapsed_ms: super::timing::elapsed_millis(started),
            error_code: result.as_ref().err().map(|error| error.error_code().into()),
        })
        .await?;
    result
}

pub(crate) async fn observe_pipeline<T>(
    runtime: Option<Arc<NativePreparationRuntime>>,
    request_id: String,
    kind: NativePreparationWorkKind,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    let Some(runtime) = runtime.filter(|runtime| runtime.active.load(Ordering::Relaxed)) else {
        return operation.await;
    };
    let work_index = runtime
        .next_work
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |index| {
            index.checked_add(1)
        })
        .map_err(|_| BitrouterError::internal("preparation work index exhausted"))?;
    observe(
        runtime.control.as_ref(),
        NativePreparationWork {
            request_id,
            kind,
            work_index,
        },
        operation,
    )
    .await
}
