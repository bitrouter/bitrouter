# OTel SDK migration spec

Status: **D1 is SUPERSEDED. Kept for its constraints, not its conclusion.**

> **Read this first.** This document's central decision — "OTLP export belongs
> in `bitrouter-sdk`" — was reversed before it reached a release, by
> [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md). The exporter now lives
> in `crates/bitrouter-telemetry`, and the SDK keeps only the contract it
> renders (`bitrouter_sdk::observe`).
>
> It was reversed on the axis this document *conceded* rather than answered.
> D1 below records that SDK placement is "measurably worse on three axes, and
> the decision is to accept that" — semver, graph position, containment cost —
> and buys all three with one convenience: cloud already takes `bitrouter-sdk`,
> so a standalone crate "would be a second published dependency delivering code
> the first one could carry." That trade was declined once
> [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) D4 made the semver cost
> **permanent**, and once its phase 0 turned the span schema into a declared
> artifact — which is what let the contract stay while the renderer left. See
> D1's own "Revisit if either fact changes" paragraph: the first fact changed.
>
> **What is still true and still worth reading**: the hard constraint (no
> `opentelemetry*` type in a public signature — now enforced against
> `bitrouter-sdk` by a stronger `feature-isolation` gate), the `tracer_clone`
> and eager-tracer-binding analysis, the feature-shape reasoning
> (`otel = ["otel-http"]`, the private `__otel-core` carrier), and the record
> of which names, targets and config keys are load-bearing. All of it moved to
> `bitrouter-telemetry` intact.
>
> **What is dead**: every sentence placing the exporter in the SDK, and the
> `sdk-public-api` job's original framing. Do not cite D1 as precedent.

Original status: PR 1 landed; the public-API guard landed on top of it.
Tracking issue #808.

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

The interop surface is the **span schema**, and it is worth being exact about
that, because the loose version of this claim — "OTLP export is the SDK's own
domain model rendered into an open standard" — does not survive contact with
the code, and it will be cited by the next PR that wants to move something in.

What is genuinely BitRouter semantics:

- the span names — `chat {inbound-model}`, `route`, per-hop `chat
  {upstream-model}`, `settle`, and `invoke_agent` / `execute_tool` on the ACP
  path;
- the `bitrouter.*` attribute vocabulary;
- the invariants that fail *silently and expensively* when a deployment
  re-derives them differently. A hop is not a `gen_ai` generation
  (`exporter.rs`): stamp `gen_ai.*` on it and every gen_ai-aware backend
  counts two generations and doubles the reported cost. `acp.rs` withholds
  `gen_ai.usage.*` because the substrate reports occupancy, not deltas.

That schema must be identical across deployments or "interop surface" means
nothing, and it is what the SDK owns. **The OTLP renderer ships alongside it
because there is exactly one renderer and it is default-off** — not because
OTLP transport, bearer refresh, batch processing or endpoint configuration are
SDK concerns in their own right.

Be honest about the ratio, so nobody mistakes the justification for a
principle. Of ~2,556 production lines under `src/otel/` (the tree is 4,425
lines, 42% of them `#[cfg(test)]`): **~43% span semantics, ~40% transport and
vendor glue, ~17% deployment configuration.** `config.rs` is 455 lines of
endpoint, headers, sampler kind, batch sizes, cardinality caps and bearer
token — deployment configuration by this document's own dichotomy.
`transport.rs`, `auth_client.rs`, `bearer.rs`, `processor_runtime.rs` (a
workaround for an upstream `opentelemetry_sdk` 0.32 interval quirk),
`cardinality.rs`, `http_layer.rs` and `subscriber.rs` contain no BitRouter
concept at all. And the semantics themselves are not purely vendor-neutral:
`exporter.rs` stamps `$screen_name` on the root `chat` span because *PostHog's*
"URL / Screen" column reads it.

None of that makes the placement wrong. It makes the *justification* narrower
than the sentence this section used to open with. The narrow version covers
everything actually here; the broad version would also cover metering,
charging and policy, which must stay out.

`MetricsRenderer` is unchanged by any of this: the SDK still owns the trait and
never the accumulator.

Three arguments that appear in the issue body were checked against the code
and **do not hold**. Do not reuse them:

- *Wiring cost* — the binary's wiring was a handful of `use` lines.
- *Crate-boundary drift* — no drift had actually occurred.
- *Version lockstep* — the OTel pins were already hoisted to
  `[workspace.dependencies]` in PR 0, which solves lockstep on its own.

#### The option this document did not pose

This section asks "does OTLP belong in the SDK?" and D2 asks "shim or clean
cut?". Neither asks the question that was actually live: **keep
`crates/bitrouter-observe` as its own published crate.** Issue #808 proposed
moving OTel into `apps/bitrouter`; that is the wrong home for a reason worth
recording — `apps/bitrouter` *is* an importable library (`[lib] name =
"bitrouter"`, no `publish = false`), so the objection is not access but weight:
its tree is 408 crates against the SDK's 154 with `otel-http`, so a consumer
taking the exporter from the binary would take sea-orm, ratatui, clap and the
whole CLI with it, on the CLI's release cadence.

But rejecting the binary does not by itself select the SDK. Against the
standalone crate, SDK placement is measurably *worse* on three axes, and the
decision is to accept that, not to deny it:

- **Semver.** `tracing_core` 0.1 and `tracing_subscriber` 0.3 are now public
  dependencies of the foundation crate, caused by one function
  (`otel::subscriber::tracing_subscriber_layer`). Both are 0.x, where every
  minor is breaking, and cargo versions crates rather than features — so a
  `tracing-subscriber` 0.4 forces a breaking `bitrouter-sdk` release even for
  consumers who never enable `otel`.
- **Graph position.** `opentelemetry` now roots at `bitrouter-sdk` with six
  in-repo dependents; under `bitrouter-observe` it had one. Any OTel bump
  invalidates the build cache for the whole downstream workspace.
- **Containment cost.** The `sdk-public-api` job, its pinned toolchain and
  tool, `public-api-deps.txt`, and most of this document exist because the
  code is now public-API-adjacent.

What buys those costs is a single consumer fact: **`bitrouter-cloud` links
`bitrouter-sdk` and enables an `otel` transport feature.** It is closed-source
and out-of-tree, so it needs the exporter from a published library crate, and
it already takes `bitrouter-sdk` — the standalone crate would be a second
published dependency delivering code the first one could carry. That, and not
the "domain model rendered into an open standard" sentence, is the load-bearing
argument for where this lives.

**Revisit if either fact changes:** if `tracing-subscriber` 0.4 ships, or if a
second in-repo consumer wants `otel` while another wants a lean build, the
standalone crate becomes the better shape again and this decision should be
re-opened rather than defended.

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

### The weak-feature mechanic — RETIRED

> **Superseded.** `otel::http_layer` no longer uses `tower-http`'s
> `TraceLayer`: it builds the ingress SERVER span as an OpenTelemetry span
> directly (see [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) D3). With the
> only consumer gone, `dep:tower-http` and the weak `tower-http?/trace` were
> both removed from `crates/bitrouter-sdk/Cargo.toml`. The section is kept
> because the mechanic is genuinely subtle and may be wanted again; nothing in
> it describes the current manifest. The `all(__otel-core, server)` gate on
> the router wrapper is unchanged — that part never depended on the mechanic,
> only on what the mechanic protected.

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
- ~~`tower-http` — the workspace pin carries `features = ["trace"]`, which is
  the very thing the weak feature exists to gate.~~ Removed with the mechanic
  above; the SDK no longer depends on `tower-http` at all.

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

~~`tower-http` is deliberately *not* on this list: `TraceLayer` never leaves
`router_wrapper`'s closure body, so it stays a private dependency.~~ Moot: the
SDK no longer depends on `tower-http` at all (see the retired mechanic above).
`router_wrapper`'s signature did change — it now takes `&OtelExporter` — but
that adds no foreign type to the public API, so the list is unaffected.

Every claim in this section is checked, not asserted. The rendered listing the
`sdk-public-api` job builds contains exactly one line mentioning `tracing` in
the SDK's whole public surface, and this is it:

```
pub fn bitrouter_sdk::otel::subscriber::tracing_subscriber_layer<S>(&bitrouter_sdk::otel::OtelExporter) -> impl tracing_subscriber::layer::Layer<S> where S: tracing_core::subscriber::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>
```

Three foreign types, two crates, no `tower_http` anywhere in the baseline —
the table is complete. The baseline spells the *defining* paths
(`tracing_subscriber::layer::Layer`, `tracing_core::subscriber::Subscriber`)
where the table spells the re-export paths callers write; they are the same
items.

Under `--all-features` the SDK's public surface reaches fifteen foreign crates
in total — `serde_core`, `schemars`, `serde_json`, `reqwest`,
`agent_client_protocol_schema`, `futures_core`, `anyhow`, `http`, `axum`,
`axum_core`, `tokio`, `rmcp`, `pin_project_lite`, `tracing_subscriber`,
`tracing_core`. Only the last two are in scope for this document; the rest
predate the OTel move and are listed here so the next reader does not mistake
the two-crate claim above for a claim about the whole crate.

## Hard constraint

**No `opentelemetry*` or `tracing_opentelemetry` type may appear in any public
`bitrouter-sdk` signature** — public struct fields, trait method signatures,
generic bounds, and type aliases included, not just function returns.

This is what lets the SDK export OTLP without making the OpenTelemetry version
part of its own semver contract. `tracer_clone` being `pub(crate)` is the
load-bearing piece; see the `acp.rs` constraint above.

### How it is enforced

The `sdk-public-api` CI job. It renders the SDK's whole public surface with
`cargo public-api -p bitrouter-sdk --all-features --simplified` — one
fully-qualified line per public item, with rustdoc's own name resolution
already applied, which is why fields, variant payloads, generic bounds,
associated types and aliases all collapse into a single grep. It then asserts
four things about that one artifact:

1. **The listing is real.** Two sentinels (`pub mod bitrouter_sdk::otel`, the
   `tracing_subscriber_layer` line) must be present. Every other gate is a
   negative assertion, and an empty or feature-less listing would satisfy them
   all vacuously.
2. **Hard gate.** No case-insensitive `opentelemetry` match anywhere in the
   listing. `tracing_opentelemetry` contains `opentelemetry`, so one pattern
   covers both crates, and folding case also catches type names like
   `OpenTelemetryLayer`.
3. **Re-export gate.** No verbatim `pub use opentelemetry…` /
   `pub use tracing_opentelemetry…` in `crates/bitrouter-sdk/src/`. See the
   known gap below for why gate 2 cannot see this on its own.
4. **Public-dependency gate.** The set of foreign crates the listing reaches
   must match the committed `crates/bitrouter-sdk/public-api-deps.txt`. A
   grep-only guard sees a forbidden type but never *unintended public API
   growth*, which is how a public foreign type gets added in the first place —
   `tracing_core` entered this crate's public surface through a generic bound
   and no `opentelemetry` grep would ever have flagged it.

   It tracks the crate *set* rather than the full item listing deliberately.
   The listing is ~8,500 lines, ~22% of them auto-derived marker impls
   (`Sync`, `Unpin`, `RefUnwindSafe`, …), so one new public struct writes six
   lines of pure noise into review and a genuine signal drowns in it. The set
   moves on exactly the semver-relevant event — the SDK gained or lost a
   public dependency, making an upstream breaking release a BitRouter breaking
   release — and stays silent for ordinary API work.

`--simplified` (omit blanket impls) is load-bearing, not cosmetic.
`opentelemetry` ships `impl<T> FutureExt for T`, which rustdoc attaches to
every documented type — without the flag the listing carries 353 lines of
`impl<T> opentelemetry::context::future_ext::FutureExt for <an SDK type>` that
this crate never authored, and gate 2 would be permanently red. The flag drops
them structurally, so no allow-list exists for a genuine leak to hide behind.

Two things are pinned, in `crates/bitrouter-sdk/public-api.pins` — the nightly
toolchain and the `cargo public-api` version. rustdoc's JSON format and
`cargo public-api`'s rendering each change across releases, and the manifest is
derived from that rendering, so an unpinned either could shift which paths the
extraction sees. The pins file sits beside the manifest, carries the
regeneration command, and is the single source of truth: the workflow reads
both values out of it rather than repeating them.

### Known gap in gate 2

`cargo public-api` renders a bare re-export under its **local** path. Adding
`pub use opentelemetry_sdk::trace::SdkTracer;` to `otel/mod.rs` prints

```
pub use bitrouter_sdk::otel::SdkTracer
```

— the origin crate appears nowhere in the line, so gate 2 passes. A re-export
whose *type name* carries "OpenTelemetry" (say `OpenTelemetrySpanExt`) does
trip it; a neutrally-named one does not. Gate 3 closes the verbatim case at
the source level. A re-export laundered through a local alias is out of reach
of any lexical check, and is left to gate 4: the added line still fails the
baseline diff, and a human has to consciously accept it to regenerate.

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
