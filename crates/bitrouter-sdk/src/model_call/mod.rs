//! The `model_call` pipeline — LLM chat / completion routing.
//!
//! This is the main BitRouter pipeline. Inbound requests on any of four wire
//! protocols ([`ApiProtocol`](bitrouter_ai::types::ApiProtocol)) are parsed into
//! a canonical [`Prompt`](bitrouter_ai::types::Prompt) by the adapters in
//! [`bitrouter_ai::protocol`], run through the flight pipeline plus an
//! interleaved stream stage, and rendered back in the inbound protocol.
//!
//! ## Pipeline stages
//!
//! 1. **Entry preparation** — pre-resolution hooks perform local auth and
//!    ingress normalization, then the pipeline freezes any named-router
//!    identity and checker bindings. Router-preparation hooks may choose the
//!    effective selector before its defaults are applied. Ordinary local
//!    policy/guardrail hooks then run before configured native checks. For
//!    requests without checks, the legacy order is retained: ordinary hooks
//!    finalize the selector before its defaults are applied.
//! 2. **Route** — after native checks allow the request, the cached
//!    [`RoutingTable`](routing::RoutingTable) resolution applies model policy and produces an
//!    ordered chain of [`RoutingTarget`](types::RoutingTarget)s, then every [`RouteHook`](hooks::RouteHook) can mutate
//!    or extend it (e.g. BYOK swaps the caller's own provider key onto a
//!    target).
//! 3. **Execute** — the [`Executor`](executor::Executor) calls the first target. On a retriable
//!    failure (5xx, timeout, 408/429) the [`FallbackPolicy`](routing::FallbackPolicy) advances to the
//!    next target. Every [`ExecutionHook`](hooks::ExecutionHook) runs on success and failure.
//! 4. **Stream stage** (interleaved when the response is streaming) —
//!    each [`StreamHook`](hooks::StreamHook) sees every canonical
//!    [`StreamPart`](bitrouter_ai::types::StreamPart) and can
//!    [`Pass`](StreamAction::Pass), [`Replace`](StreamAction::Replace), or
//!    [`Abort`](StreamAction::Abort) it.
//! 5. **Settle** — every registered [`SettlementRecorder`](settlement::SettlementRecorder) runs in
//!    registration order against the immutable [`SettlementContext`](settlement::SettlementContext).
//!    Deployments use recorders for metering, charging, signed receipts,
//!    etc.; the SDK is opinionated only about pipeline-data correctness.
//! 6. **Observe** — every [`ObserveHook`](hooks::ObserveHook) sees phase boundaries and the final
//!    [`RequestOutcome`](hooks::RequestOutcome); observers are read-only and error-swallowing.
//!
//! ## Building a pipeline
//!
//! The usual entry point is [`crate::App::builder`] → `.model_call(...)`
//! sub-builder, which exposes [`PipelineBuilder`](builder::PipelineBuilder):
//!
//! ```no_run
//! use std::sync::Arc;
//! use bitrouter_sdk::App;
//! use bitrouter_sdk::model_call::executor::HttpExecutor;
//! use bitrouter_sdk::model_call::routing::StaticRoutingTable;
//!
//! # fn run() -> bitrouter_sdk::Result<()> {
//! let executor = HttpExecutor::with_defaults()?;
//! let app = App::builder()
//!     .model_call(|lm| {
//!         lm.routing_table(Arc::new(StaticRoutingTable::new()))
//!           .executor(Arc::new(executor));
//!     })
//!     .build()?;
//! # let _ = app;
//! # Ok(()) }
//! ```
//!
//! ## Protocol isolation
//!
//! The hook traits here are **not** shared with [`crate::mcp`] / [`crate::acp`]:
//! an `mcp::RouteHook` cannot be registered on a `model_call::pipeline::Pipeline`
//! (compile-time error). Cross-cutting reuse goes through crate-root library
//! code, never a shared trait.

pub mod builder;
pub mod context;
pub mod executor;
pub mod hooks;
pub mod operations;
pub mod pipeline;
pub mod request_checks;
pub mod routing;
pub mod server_tools;
pub mod settlement;
pub mod stream;
pub mod timing;
pub mod types;

#[cfg(test)]
mod tests;
