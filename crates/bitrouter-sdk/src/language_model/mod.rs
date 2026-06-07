//! The `language_model` pipeline — LLM chat / completion routing.
//!
//! This is the main BitRouter pipeline. Inbound requests on any of four wire
//! protocols ([`ApiProtocol`]) are parsed into a canonical [`Prompt`] by the
//! adapters in [`protocol`], run through a four-stage flight pipeline plus an
//! interleaved stream stage, and rendered back in the inbound protocol.
//!
//! ## Pipeline stages
//!
//! 1. **Pre-request** — every [`PreRequestHook`] runs in registration order.
//!    Each returns a [`HookDecision`] of [`Allow`](HookDecision::Allow) or
//!    [`Deny`](HookDecision::Deny). The first deny short-circuits the pipeline.
//! 2. **Route** — the [`RoutingTable`] resolves the request's `model` into an
//!    ordered chain of [`RoutingTarget`]s, then every [`RouteHook`] can mutate
//!    or extend it (e.g. BYOK swaps the caller's own provider key onto a
//!    target).
//! 3. **Execute** — the [`Executor`] calls the first target. On a retriable
//!    failure (5xx, timeout, 408/429) the [`FallbackPolicy`] advances to the
//!    next target. Every [`ExecutionHook`] runs on success and failure.
//! 4. **Stream stage** (interleaved when the response is streaming) —
//!    each [`StreamHook`] sees every canonical [`StreamPart`] and can
//!    [`Pass`](StreamAction::Pass), [`Replace`](StreamAction::Replace), or
//!    [`Abort`](StreamAction::Abort) it.
//! 5. **Settle** — every registered [`SettlementRecorder`] runs in
//!    registration order against the immutable [`SettlementContext`].
//!    Deployments use recorders for metering, charging, signed receipts,
//!    etc.; the SDK is opinionated only about pipeline-data correctness.
//! 6. **Observe** — every [`ObserveHook`] sees phase boundaries and the final
//!    [`RequestOutcome`]; observers are read-only and error-swallowing.
//!
//! ## Building a pipeline
//!
//! The usual entry point is [`crate::App::builder`] → `.language_model(...)`
//! sub-builder, which exposes [`PipelineBuilder`]:
//!
//! ```no_run
//! use std::sync::Arc;
//! use bitrouter_sdk::App;
//! use bitrouter_sdk::language_model::{HttpExecutor, StaticRoutingTable};
//!
//! # fn run() -> bitrouter_sdk::Result<()> {
//! let app = App::builder()
//!     .language_model(|lm| {
//!         lm.routing_table(Arc::new(StaticRoutingTable::new()))
//!           .executor(Arc::new(HttpExecutor::with_defaults().unwrap()));
//!     })
//!     .build()?;
//! # let _ = app;
//! # Ok(()) }
//! ```
//!
//! ## Protocol isolation
//!
//! The hook traits here are **not** shared with [`crate::mcp`] / [`crate::acp`]:
//! an `mcp::RouteHook` cannot be registered on a `language_model::Pipeline`
//! (compile-time error). Cross-cutting reuse goes through crate-root library
//! code, never a shared trait.

pub mod auth;
pub mod builder;
pub mod context;
pub mod executor;
pub mod hooks;
pub mod pipeline;
pub mod protocol;
pub mod routing;
pub mod settlement;
pub mod stream;
pub mod types;

#[cfg(test)]
mod tests;

// ===== canonical re-exports — `language_model::Pipeline`, etc. =====

pub use auth::{AuthApplier, AuthAppliers};
pub use builder::PipelineBuilder;
pub use context::{PipelineContext, StreamContext};
pub use executor::{
    DispatchExecutor, Executor, HttpExecutor, HttpTimeouts, MockExecutor, MockResponse,
    StreamPartStream,
};
pub use hooks::{
    DenyReason, ExecutionHook, FallbackDecision, HookDecision, HopOutcome, ObserveHook, Phase,
    PreRequestHook, RequestOutcome, RouteHook, StreamHook,
};
pub use pipeline::{DEFAULT_KEEPALIVE, Pipeline};
pub use protocol::{
    InboundAdapter, OutboundAdapter, OutboundDispatch, SseEvent, StreamDecoder, StreamEncoder,
    Transport, inbound_adapter_for, sanitize_model_name,
};
pub use routing::{
    DefaultFallbackPolicy, FallbackPolicy, ModelInfo, RoutingPrefs, RoutingTable, SortOrder,
    StaticRoutingTable,
};
pub use settlement::{SettlementContext, SettlementRecorder};
pub use stream::{
    SseFrame, SseKeepaliveStream, StreamAction, StreamInterest, StreamOutcome, StreamProcessor,
    UsageAccumulator,
};
pub use types::{
    ApiProtocol, Capability, Content, ExecutionResult, FinishReason, GenerateResult,
    GenerationParams, Message, PipelineRequest, PipelineResponse, Prompt, Role, RoutingTarget,
    StreamPart, Tool, Usage,
};
