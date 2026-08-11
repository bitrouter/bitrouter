# OTel SDK migration spec

Status: **PR 1 landed.** Tracking issue #808.

Moves the OTLP exporter out of `crates/bitrouter-observe` into
`crates/bitrouter-sdk` behind an `otel` feature, and deletes the observe
crate. This document records the decisions and the constraints that forced
them, so the next reader does not relitigate settled ground or "tidy" a name
that is load-bearing.

## Decisions

### D1 — OTLP export belongs in the SDK

**Yes.** The rule, stated positively:

> **Interop surfaces ship in the SDK behind default-off features; deployment
> business logic does not.**

OTLP export is the SDK's own domain model rendered into an open standard.
Which span is `chat`, what counts as a hop, when settlement closes — that is
BitRouter semantics, not vendor glue, and it must be identical across
deployments or "interop surface" means nothing. The `MetricsRenderer` seam is
unchanged: the SDK still owns the trait and never the accumulator.

Three arguments that appear in the issue body were checked against the code
and **do not hold**. Do not reuse them:

- *Wiring cost* — the binary's wiring was a handful of `use` lines.
- *Crate-boundary drift* — no drift had actually occurred.
- *Version lockstep* — the OTel pins were already hoisted to
  `[workspace.dependencies]` in PR 0, which solves lockstep on its own.

### D2 — one-shot cut

No overlap release and no re-export shim; `crates/bitrouter-observe` is
deleted in the same PR.

A shim is perfectly buildable — it would be `pub use bitrouter_sdk::otel::acp;`
plus `pub mod otel { pub use bitrouter_sdk::otel::*; }`, and since both
`AcpSpanRecorder` and `tracing_subscriber_layer` now live in the SDK it would
re-export them wholesale without ever touching `tracer_clone`. It is rejected
as *undesirable*, not impossible:

- it means publishing a crate whose entire body is a re-export;
- it buys a deprecation cycle for a pre-1.0 alpha, where the compatibility
  promise it would honour does not exist yet;
- crates.io's index is append-only, so any consumer pinned to a released
  `bitrouter-observe` keeps building whether or not a shim ships. The shim
  would serve only consumers who want to *upgrade* without editing a `use`
  line — a group that, at alpha, is not worth a permanent crate for.

Do not reach for the `tracer_clone` argument here. It is load-bearing for
*where `acp.rs` lands* (see the constraint below) and says nothing about shim
feasibility; the two were conflated in the issue briefing.

### D3 — `otel = ["otel-http"]` plus a private `__otel-core`

The transport-less core must never be the name a user reaches for, because on
its own it does not compile — it is a `compile_error!` by construction.

| Feature        | Role                                                          |
| -------------- | ------------------------------------------------------------- |
| `otel`         | Public entry point. Always builds; selects the default transport. |
| `otel-http`    | OTLP/HTTP + protobuf over reqwest + rustls.                    |
| `otel-grpc`    | OTLP/gRPC over tonic + native trust roots.                     |
| `__otel-core`  | Private carrier for the transport-agnostic stack.              |

`OTEL_ENABLED` is deliberately declared *outside* the gate: it answers "is the
exporter compiled in?", so it has to exist in every build.

## Constraints discovered during the move

### `acp.rs` must live in the SDK, not the binary

The original issue proposed moving `acp.rs` into `apps/bitrouter`. That is
impossible. `AcpSpanRecorder::new` gets its tracer from
`OtelExporter::tracer_clone()`, which is `pub(crate)` specifically so the
tracer type does not escape, and the recorder *stores* that tracer as a struct
field. Across a crate boundary this forces `tracer_clone` public.

It therefore lands at `crates/bitrouter-sdk/src/otel/acp.rs`, beside the
exporter, and `tracer_clone` stays crate-private.

### `http_layer.rs` splits in two

The original file mixed an axum-free helper with an axum router wrapper.
Gating the whole file on `server` would force the axum HTTP server feature on
consumers (e.g. `bitrouter-cloud`) that want only the tracing bridge.

| Module               | Gate                          | Contents                                                        |
| -------------------- | ----------------------------- | --------------------------------------------------------------- |
| `otel::subscriber`   | `__otel-core`                 | `tracing_subscriber_layer` — the `tracing` ↔ OTel bridge.        |
| `otel::http_layer`   | `__otel-core` **and** `server` | `router_wrapper` + the inbound SERVER span callbacks.            |

### The weak-feature mechanic

`server` carries `tower-http?/trace`. The `?` makes it **weak**: it activates
tower-http's `trace` feature *only if* the SDK's own `dep:tower-http` was
already activated (by `__otel-core`). It never pulls the dependency in itself.

| Features enabled | tower-http `trace` | axum |
| ---------------- | ------------------ | ---- |
| `server`         | no                 | yes  |
| `otel-http`      | no                 | no   |
| `otel`, `server` | **yes**            | yes  |

That is exact feature-*intersection* semantics, which plain Cargo features
cannot express, and it is what makes the `all(__otel-core, server)` gate on
the router wrapper work without over-pulling in either direction.

Two dependencies deliberately do **not** take the workspace pin, for reasons
recorded inline in `crates/bitrouter-sdk/Cargo.toml`:

- `tracing-subscriber` — the workspace pin carries `env-filter`, which drags
  in matchers / regex-automata / regex-syntax. `otel::subscriber` needs only
  `LookupSpan`.
- `tower-http` — the workspace pin carries `features = ["trace"]`, which is
  the very thing the weak feature exists to gate.

### `tracing_subscriber_layer`'s return type

```rust
pub fn tracing_subscriber_layer<S>(exporter: &OtelExporter) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
```

Two things here are easy to "fix" and must not be:

- **No `+ Send + Sync + 'static` on the return type.**
  `OpenTelemetryLayer<S, T>` holds a `PhantomData<S>`, so spelling those auto
  traits forces `S: Send + Sync` into the where-clause — a public bound
  narrowing. RPIT auto-trait leakage already delivers `Send`/`Sync` to callers
  whose `S` has them, so bare `impl Layer<S>` is strictly less breaking.
- **No `Layer::boxed()`.** `OpenTelemetrySpanExt::{set_parent, context}` rely
  on `downcast_raw` finding `WithContext`. Boxing puts that behind a
  forwarding impl and silently breaks SERVER → `chat` span parenting — with no
  error, just orphaned traces.

This signature makes **two crates semver-committed public dependencies** of
the SDK. Three foreign types are reachable from it:

| Type                                        | From                 |
| ------------------------------------------- | -------------------- |
| `tracing_subscriber::Layer`                 | tracing-subscriber 0.3 |
| `tracing_subscriber::registry::LookupSpan`  | tracing-subscriber 0.3 |
| `tracing_core::Subscriber`                  | tracing-core 0.1      |

`tracing_core::Subscriber` is easy to miss because the bound is spelled
`tracing::Subscriber` — `tracing` re-exports it, but the semver commitment is
to **tracing-core**, which is where the trait is defined and where a breaking
change to it would originate. Both commitments are accepted; the hard
constraint below covers `opentelemetry*` only.

`tower-http` is deliberately *not* on this list: `TraceLayer` never leaves
`router_wrapper`'s closure body, so it stays a private dependency.

## Hard constraint

**No `opentelemetry*` or `tracing_opentelemetry` type may appear in any public
`bitrouter-sdk` signature** — public struct fields, trait method signatures,
generic bounds, and type aliases included, not just function returns.

This is what lets the SDK export OTLP without making the OpenTelemetry version
part of its own semver contract. `tracer_clone` being `pub(crate)` is the
load-bearing piece; see the `acp.rs` constraint above.

Enforcement is by inspection today. The machine-checkable gate (a
public-API-surface diff) needs a nightly toolchain and is deliberately a
separate future PR.

## Frozen: the `bitrouter-observe` config namespace

`bitrouter-observe` is also a **user-facing configuration namespace**, and that
namespace outlives the crate. `config.plugins` is an unvalidated
`HashMap<String, Value>`, so a renamed key still parses — telemetry would
silently stop, with no error anywhere. These strings must not change:

- `plugins.bitrouter-observe` and every `bitrouter-observe` YAML key in
  `apps/bitrouter/src/assemble.rs`
- the `BITROUTER_OBSERVE_*` env var names in
  `crates/bitrouter-sdk/src/otel/config.rs`
- the `/metrics` migration banner and the onboarding notice
- the `e2e.rs` assertion that the banner contains `bitrouter-observe.otel`
- the YAML fixtures in `apps/bitrouter/tests/{observe_hierarchy,full_stack}.rs`
- `skills/bitrouter/references/cli.md`

Equally frozen, for the same wire-contract reason: the `io.bitrouter.observe`
instrumentation scope name, the `bitrouter` meter name, and the explicit
`target:` values `bitrouter::observe::http` / `bitrouter::observe::cardinality`.
The targets are namespaced as a *subsystem*, not a crate, which is why they
survived the move unchanged.

## Enforcement

`apps/bitrouter/tests/observe_hierarchy.rs` is the behavioural proof: it
asserts the full SERVER → `chat` → (`route`, per-hop `chat`, `settle`)
hierarchy against a live OTLP collector stub. The relocation is correct only
if that test passes with import-path changes alone.

Its `EnvFilter` is fixed at
`"info,bitrouter_observe=warn,bitrouter_sdk=warn"`. The `bitrouter_observe`
directive is inert now that the crate is gone, and is kept on purpose: the
filter is a regression guard for the explicit `target:` pins, not a
description of the current layout.

The `feature-isolation` CI job enforces the dependency invariants. `cargo
tree` mishandles a bad name in two different ways, and the job guards both:

- **`-i <pkg>`** exits 101 with a byte-identical message whether the package is
  absent from the selected tree **or** matches no package at all, so a
  misspelled name would pass silently forever. Every negative assertion is
  therefore preceded by a positive one proving the name resolves under
  `--all-features`.
- **`-p <crate>`** is the sharper edge: an unknown package spec is *silently
  ignored* and `cargo tree` falls back to the whole workspace default tree,
  exiting **0** with a full tree on stdout and nothing on stderr. Exit status
  alone cannot catch it. The crate loop therefore asserts workspace membership
  with `cargo metadata` before feeding any name to `-p`.
