//! The observability **contract** — what BitRouter promises about its own
//! telemetry, independent of any renderer of it.
//!
//! This module carries no dependency beyond `serde` and is behind no feature
//! gate, and both facts are load-bearing. The contract is not optional: a
//! deployment that renders BitRouter's telemetry — through the OTLP exporter
//! in `bitrouter-telemetry`, or through anything else — implements what is
//! declared here, and it must be able to read the declaration without opting
//! into a renderer it is not using.
//!
//! Two things live here:
//!
//! - [`schema`] — the span schema as a declaration rather than as call sites:
//!   span names, the `bitrouter.*` attribute vocabulary, requiredness, the
//!   metrics, the extension region, and the invariants that fail silently and
//!   expensively when a deployment re-derives them differently. It renders to
//!   the committed artifact `crates/bitrouter-sdk/span-schema.json`, which
//!   ships with the crate.
//! - [`SpanAttributes`] — the bounded hatch through which a deployment adds
//!   attributes the schema does not declare.
//!
//! What does *not* live here is the emission of any of it. The SDK owns the
//! contract and the [`ObserveHook`](crate::language_model::ObserveHook) seam
//! observers plug into; rendering the observations onto a wire is optional
//! egress and ships in its own crate. See `docs/TELEMETRY_CRATE_SPEC.md`.

pub mod schema;
mod span_attributes;

pub use span_attributes::SpanAttributes;
