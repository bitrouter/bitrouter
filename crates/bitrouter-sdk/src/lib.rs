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
//!   - `otel` — OTLP export of the pipeline's spans and metrics
//!     ([`otel::OtelExporter`], [`otel::OtelObserveHook`]), over OTLP/HTTP by
//!     default or OTLP/gRPC under `otel-grpc`. No `opentelemetry*` type
//!     appears in this crate's public API.
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
//! ## What ships here, and what ships elsewhere
//!
//! The dividing line is **interop surfaces ship in the SDK behind default-off
//! features; deployment business logic does not.**
//!
//! What puts [`otel`] on the SDK side of that line is the **span schema**, not
//! the exporter: the span names (`chat`, `route`, `settle`, the per-hop
//! `chat`), the `bitrouter.*` attribute vocabulary, and the invariants that
//! fail silently and expensively when a deployment gets them wrong — a hop is
//! not a `gen_ai` generation, and stamping it as one makes every
//! gen_ai-aware backend double-count the reported cost. That schema has to be
//! identical across every deployment or "interop surface" means nothing.
//!
//! The OTLP renderer ships *with* the schema because there is exactly one
//! renderer and it is default-off — not because transport, credentials,
//! batch processing and endpoint configuration are themselves SDK concerns.
//! They are here as the schema's implementation, and they are most of the
//! module by volume. Do not read this paragraph as licence to move further
//! deployment logic in; read it as the narrowest justification that covers
//! what is already here.
//!
//! An out-of-tree consumer that skips the feature pays nothing: it is off by
//! default and the whole OpenTelemetry stack drops out of the dependency tree
//! with it (124 crates without, 154 with). That is a claim about *this
//! crate*, not about the workspace — `apps/bitrouter` enables `otel-http`
//! unconditionally, so a workspace build resolves one `bitrouter-sdk` node
//! with the stack on and every in-repo consumer links it.
//!
//! One shared library plugin still lives in its own crate:
//!
//! - `bitrouter-guardrails` — request / response content scanning (block +
//!   redact). Content policy is a deployment's own call, not a wire standard.
//!
//! Everything else in that category (auth, policy, charging, metering) is
//! **deployment-specific business logic, not shared library code**. The OSS
//! `apps/bitrouter` binary provides its own implementations under
//! `apps/bitrouter/src/{auth,policy,metering}/`. Closed-source deployments
//! (e.g. a cloud product) write their own `PreRequestHook` /
//! `SettlementRecorder` impls against the SDK's stable traits.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// The observability stack is transport-agnostic but cannot function without a
// wire transport. `__otel-core` carries the stack; `otel-http` / `otel-grpc`
// add a transport. Guard against `__otel-core` being enabled on its own (e.g.
// a downstream typo or a stray `dep:` activation) with a clear message instead
// of a wall of "cannot find function `span_exporter`" errors.
#[cfg(all(
    feature = "__otel-core",
    not(any(feature = "otel-http", feature = "otel-grpc"))
))]
compile_error!(
    "the OpenTelemetry stack needs a transport: enable `otel-http` for \
     OTLP/HTTP, or `otel-grpc` for OTLP/gRPC"
);

// ===== shared library code (crate root) =====
pub mod app;
pub mod caller;
pub mod error;
pub mod event;
pub mod metrics;
pub mod plugin;
pub mod url_validator;

#[cfg(feature = "config_file")]
#[cfg_attr(docsrs, doc(cfg(feature = "config_file")))]
pub mod config;

#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub mod server;

#[cfg(feature = "__otel-core")]
#[cfg_attr(docsrs, doc(cfg(feature = "otel")))]
pub mod otel;

/// Whether the OpenTelemetry exporter is compiled in (under any transport).
pub const OTEL_ENABLED: bool = cfg!(feature = "__otel-core");

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
