//! OpenTelemetry GenAI exporter for BitRouter observability events.
//!
//! Wiring overview:
//!
//! ```text
//! observer.rs   <─ implements the three callback traits from `bitrouter-core`.
//!     │           Builds a `Span` from each event, sourced through `semconv`.
//!     ▼
//! semconv.rs    <─ maps BitRouter event fields to OTel GenAI attribute names.
//!                 Single point of impact when GenAI semconv stability advances
//!                 from `Development`.
//!     │
//!     ▼
//! pipeline.rs   <─ for each configured destination: deterministic sampling on
//!                 `gen_ai.conversation.id`, per-destination redact, then
//!                 `export(span)`.
//! ```
//!
//! In this layer, [`pipeline::Pipeline::export`] emits a structured
//! `tracing::info!` event instead of an OTLP/HTTP request. A follow-up commit
//! plugs in the `opentelemetry-otlp` `BatchSpanProcessor` against the same
//! [`pipeline::PipelineConfig`] surface, with the only change being inside
//! [`pipeline::Pipeline::export`].

pub mod observer;
pub mod pipeline;
pub mod semconv;
pub mod span;
