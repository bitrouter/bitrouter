//! # bitrouter-telemetry
//!
//! Optional telemetry egress for BitRouter: it renders the observability
//! contract declared in [`bitrouter_sdk::observe`] onto a wire and ships it
//! somewhere. It binds to that contract; it never defines it.
//!
//! Today there is exactly one renderer — [`otel`], an OpenTelemetry exporter
//! with multi-tenant attribution, over OTLP/HTTP (`otel-http`, the default) or
//! OTLP/gRPC (`otel-grpc`). It plugs into the SDK as an ordinary
//! [`ObserveHook`](bitrouter_sdk::language_model::ObserveHook), which is a
//! seam with more than one production implementation: the OSS binary registers
//! its own observers alongside this one.
//!
//! ## Which word means what
//!
//! One rule covers the whole surface, and it is worth learning once:
//!
//! > **`telemetry` is what an operator configures. `observe` is what BitRouter
//! > promises and how it identifies itself on the wire.**
//!
//! So an operator configures this crate through `plugins.bitrouter-telemetry`
//! and `BITROUTER_TELEMETRY_*`, while `io.bitrouter.observe` (the
//! instrumentation scope downstream dashboards select on), the
//! `bitrouter::observe::*` `RUST_LOG` targets, and
//! [`bitrouter_sdk::observe`] (the contract) keep the other word — permanently,
//! and for reasons that have nothing to do with tidiness. See
//! `docs/TELEMETRY_CRATE_SPEC.md` D5 and D6.
//!
//! The one nested `telemetry` key — `plugins.bitrouter-telemetry.telemetry` —
//! is not a competing scope; it is an endpoint preset that points the exporter
//! at BitRouter's own first-party endpoint, one destination choice among
//! several this crate can export to.
//!
//! ## What ships here and what does not
//!
//! Here: transport selection, credentials and bearer refresh, batch and
//! periodic-reader runtimes, endpoint and sampler configuration, cardinality
//! limiting, span and metric construction, the inbound HTTP ingress span, and
//! the `tracing` ↔ OpenTelemetry bridge.
//!
//! Not here: the span names, the `bitrouter.*` attribute vocabulary, the
//! invariants, or the extension region. Those are
//! [`bitrouter_sdk::observe::schema`] and the committed
//! `crates/bitrouter-sdk/span-schema.json` rendered from it, and this crate's
//! conformance tests assert that what it emits matches them.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// The OpenTelemetry stack is transport-agnostic but cannot function without a
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

// `server` gates the ingress span, which lives inside the `otel` module — so
// `server` without a transport compiles nothing at all while still pulling
// axum, http-body, pin-project-lite and `bitrouter-sdk/server`. That is a
// silent no-op, and silent no-op features are how a build ends up carrying
// weight nobody can account for. Fail the same way the transport guard does.
#[cfg(all(feature = "server", not(feature = "__otel-core")))]
compile_error!(
    "`server` only adds the inbound ingress span to the OTLP exporter, so it \
     does nothing without a transport: enable `otel` (or `otel-http` / \
     `otel-grpc`) alongside it"
);

#[cfg(feature = "__otel-core")]
#[cfg_attr(docsrs, doc(cfg(feature = "otel")))]
pub mod otel;

/// Whether the OpenTelemetry exporter is compiled in (under any transport).
///
/// The binary reports this through `bitrouter observe status` so an operator
/// can tell "the feature is off" from "the daemon is down".
pub const OTEL_ENABLED: bool = cfg!(feature = "__otel-core");
