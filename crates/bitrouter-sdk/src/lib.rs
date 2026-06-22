//! # bitrouter-sdk
//!
//! The BitRouter SDK: build a programmable router for LLM API traffic.
//! Inbound requests on any of four wire protocols (Chat Completions,
//! Responses, Messages, Generate Content) are normalised into a
//! canonical pipeline, run through a chain of hooks (auth, policy, settlement,
//! guardrails, observability), dispatched to the right upstream provider, and
//! rendered back in the inbound protocol — so a client written for one
//! provider can transparently use any other.
//!
//! ## What's in the SDK
//!
//! - **Three independent protocol pipelines** — one per wire family:
//!   - [`language_model`] — the main pipeline. Handles LLM completions with the
//!     full hook set (pre-request → route → execute → settle, plus an
//!     interleaved stream stage and read-only observation).
//!   - [`mcp`] — Model Context Protocol routing (pure routing, no settlement).
//!   - [`acp`] — Agent Client Protocol routing (pure routing, no settlement).
//!
//!   The pipelines are deliberately **not** generic over a shared hook trait:
//!   each one has its own hooks so a stage in `language_model` can't be
//!   accidentally registered on `mcp`. Cross-cutting reuse goes through the
//!   crate-root library code below, never a shared trait.
//!
//! - **Shared crate-root infrastructure** that every protocol uses:
//!   - [`app`] — [`App`] / [`AppBuilder`] / [`Plugin`].
//!   - [`error`] — the unified [`BitrouterError`] / [`Result`].
//!   - [`caller`] — [`CallerContext`] (identity-only; business
//!     classifications like payment method live in deployment code, not
//!     here).
//!   - [`event`] — typed [`PipelineEvent`] bus.
//!   - [`metrics`] — the [`MetricsRenderer`] trait (the `GET /metrics`
//!     endpoint contract; spend / token / rate aggregation are
//!     deployment-specific concerns).
//!   - [`plugin`] — [`PluginId`] and SQL [`MigrationItem`]s.
//!
//! - **Optional features** (off by default):
//!   - `server` — an [axum] HTTP front-end ([`server::build_router`],
//!     [`App::serve`]) wiring all four inbound protocols, plus
//!     `GET /metrics`, `POST /mcp/{server}`, and graceful shutdown.
//!   - `config_file` — YAML config loading ([`config::load`],
//!     [`config::ConfigRoutingTable`]).
//!
//! [axum]: https://docs.rs/axum
//!
//! ## Anatomy of a request
//!
//! For the LLM pipeline (`language_model`):
//!
//! 1. **Pre-request** — every [`PreRequestHook`] runs; auth, policy, and
//!    upstream guardrails reject early. Returns
//!    [`HookDecision::Allow`] or denies.
//! 2. **Route** — a [`RoutingTable`] resolves the `model` field into an
//!    ordered chain of [`RoutingTarget`]s; every
//!    [`RouteHook`](language_model::RouteHook) can mutate it (e.g. BYOK swaps
//!    in the caller's own provider key).
//! 3. **Execute** — the [`Executor`](language_model::Executor) calls the first
//!    target. On a retriable failure the [`FallbackPolicy`] advances to the
//!    next target. Streaming responses run through every
//!    [`StreamHook`](language_model::StreamHook) on each canonical part.
//! 4. **Settle** — every registered
//!    [`SettlementRecorder`](language_model::SettlementRecorder) runs in
//!    registration order against the immutable
//!    [`SettlementContext`](language_model::SettlementContext). Deployments
//!    use recorders for metering, charging, signed receipts, etc.; the SDK
//!    is opinionated only about pipeline-data correctness.
//! 5. **Observe** — [`ObserveHook`](language_model::ObserveHook)s see every
//!    phase boundary and the final outcome; they never influence the request.
//!
//! See each hook trait's docs for the exact contract.
//!
//! ## Building an `App`
//!
//! At minimum a `language_model` pipeline needs a routing table and an
//! executor:
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
//! Shared library plugins implement one or more hook traits from this SDK
//! and install themselves through [`AppBuilder::plugin`] (a convenience
//! that drops their hooks into the right sub-builder; hooks can equally be
//! registered one-by-one without [`Plugin`]).
//!
//! With the `server` feature on, `app.serve("0.0.0.0:4356")` wires the
//! whole router and runs it until SIGTERM.
//!
//! ## What ships in adjacent crates
//!
//! Two shared library plugins in this repo:
//!
//! - `bitrouter-observe` — Prometheus exporter + OTLP/HTTP traces.
//! - `bitrouter-guardrails` — request / response content scanning (block +
//!   redact).
//!
//! Anything else (auth, policy, charging, metering) is **deployment-specific
//! business logic, not shared library code**. The OSS `apps/bitrouter`
//! binary provides its own implementations under
//! `apps/bitrouter/src/{auth,policy,metering}/`. Closed-source deployments
//! (e.g. a cloud product) write their own `PreRequestHook` /
//! `SettlementRecorder` impls against the SDK's stable traits.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// ===== shared library code (crate root) =====
pub mod app;
pub mod caller;
pub mod error;
pub mod event;
pub mod metrics;
pub mod plugin;
pub mod url_validator;

#[cfg(feature = "config_file")]
pub mod config;

#[cfg(feature = "server")]
pub mod server;

// ===== per-protocol modules =====
pub mod acp;
pub mod language_model;
pub mod mcp;

pub use app::{App, AppBuilder, Plugin, PromptTransform};
// Re-exported so downstream `PromptTransform` impls can name the header map
// passed to `apply_with_headers` without taking a direct `http` dependency.
pub use caller::CallerContext;
pub use error::{BitrouterError, Result};
pub use event::{EventBus, PipelineEvent};
pub use http::HeaderMap;
pub use language_model::{
    FallbackPolicy, HookDecision, PreRequestHook, RoutingTable, RoutingTarget,
};
pub use metrics::MetricsRenderer;
pub use plugin::{MigrationContent, MigrationItem, PluginId};
