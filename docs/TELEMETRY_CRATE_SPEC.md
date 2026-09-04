# Telemetry crate spec

Status: **built.** Reverses `OTEL_SDK_MIGRATION_SPEC.md` D1 — the OTLP renderer
has left `bitrouter-sdk` for `crates/bitrouter-telemetry`. Landed inside the
window described below, on PR #809's own branch, so cloud migrates once rather
than twice.

Keeps everything `OTEL_TIERING_SPEC.md` phases 0 and 2 built. This is a
placement decision, not a rewrite: the span schema artifact, the OTel-native
ingress span, the conformance tests and the `sdk-public-api` job all survive
unchanged in content.

| Decision | State |
| --- | --- |
| **D1** — the OTLP renderer ships as its own crate, not as an SDK feature | **done** |
| **D2** — the cut is the whole `otel/` module minus the contract tier | **done.** The contract tier is `bitrouter_sdk::observe`, ungated |
| **D3** — the crate is named `bitrouter-telemetry` | **done.** Its naming collision is resolved by D6, not tolerated |
| **D4** — PR #809 is redirected in place, not merged-then-reverted | **done** |
| **D5** — what is frozen, and what only looked frozen | **amended.** The wire scope, meter name and `RUST_LOG` targets are frozen permanently; the config surface was not contract and is renamed |
| **D6** — rename the config surface behind an unknown-key guard | **added after the crate landed** |

**The load-bearing measurement came out exactly as predicted**, and it was the
one flagged in *Evidence* as a prediction rather than a result:
`crates/bitrouter-sdk/public-api-deps.txt` lost **exactly** `tracing_core` and
`tracing_subscriber`, and nothing else, regenerated on the pinned
`nightly-2026-05-05` / `cargo-public-api 0.52.0`. See *As built*.

## The arguments that are dead

`OTEL_TIERING_SPEC.md` withdrew a version of this proposal (its phase 3, D1
option 1: *"tier 3 becomes its own thin published crate (`bitrouter-otel`)"*).
It withdrew it on measurement, and those measurements stand. **This document
does not reuse any of them, and a review that reintroduces them is reviewing
the wrong argument.**

- **Crate count.** `default = []`, so no consumer pays the +42 today. The
  tiering spec's own ledger shows nobody gets lighter under the move: the app
  wants OTLP, cloud wants OTLP, and a schema-only consumer is already at +0.
  Do not quote 30, 42, or 36.
- **Build-cache position.** Measured at roughly zero: 22.8s to invalidate the
  whole OTel family against 10.9s for the residue that stays, inside a build
  where `apps/bitrouter` alone costs 27.3s and rebuilds in every scenario.
  Exactly nil on a cold CI cache.
- **Wiring cost, crate-boundary drift, version lockstep.** All three were
  checked against the code in the #808 review and do not hold. The OTel pins
  are already hoisted to `[workspace.dependencies]`.
- **Discoverability.** #808's opening complaint — *"a satellite crate a
  consumer has to discover on their own"* — is answered by
  `span-schema.json` and the docs, not by co-location, and the wiring it
  described was a handful of `use` lines.

What is left after subtracting all of that is three arguments, none of which
the tiering spec's *cloud question* section measured, because none of them is
about weight. The withdrawal anticipated exactly this and left the door on the
latch:

> Reopen only on the trigger in *Abandon triggers*, or if `bitrouter-otel` is
> wanted as a **published product surface** that consumers adopt without the
> SDK. That is a positioning decision needing different evidence than this
> section gathered, and it should not borrow this section's arguments.

## Why

### 1. The boundary is being defended in prose in three files

PR #809 rewrote the doctrine text carefully and correctly. That is the finding,
not a complaint. Read what the careful version has to say:

- `sdk/lib.rs` — *"Do not read this paragraph as licence to move further
  deployment logic in; read it as the narrowest justification that covers what
  is already here."*
- `sdk/metrics.rs` — *"The SDK's push path is a different kind of thing and
  does not weaken the rule here."*
- `docs/DEVELOPMENT.md` — *"That ratio is the price of keeping the one renderer
  next to the schema it renders. It is not a precedent."*

Three files, three separate warnings to the reader not to generalise the rule
they just read. A boundary that needs to be restated defensively in three
places is one the code has stopped expressing. The prose is doing the work a
crate boundary used to do for free, and prose does not fail the build.

The ratio those paragraphs are apologising for is the SDK's own:
`DEVELOPMENT.md` puts ~40% of `otel/`'s production lines at transport and
vendor glue and another ~17% at deployment configuration, against ~43% span
semantics. The document that states the rule concedes that 57% of the code
admitted under it does not satisfy it.

### 2. The name was the defect, and the chain misdiagnosed it

Issue #808's third argument was that the crate boundary *hid drift*:
`sdk/lib.rs` called observe a "Prometheus exporter" and `sdk/metrics.rs`
referenced a `PrometheusHook` that no longer existed. The boundary did not hide
that. The **name** did — `bitrouter-observe` claims a domain it never owned, so
a reader writing the SDK's observability doc reached for it as the
observability implementation, and was wrong.

The tree says the app owns observability and always did:

- `assemble.rs:649-651` registers **two** `ObserveHook`s on the pipeline:
  `OtelObserveHook` from the crate, and `PredictiveResponseObserver`
  (`workflow_state/response_observer.rs:206`), which is the app's own.
  `ObserveHook` is not the OTel seam; it is a seam with two production
  implementations, one of them BitRouter's.
- `assemble.rs:1219` implements `MetricsRenderer` in the app, not in the crate.
- `metering/`, `trajectory/` and `adequacy/` — ~25 files of spend recording,
  durable request history, replay and a reliability ledger — are the app's own
  observation plane and were never candidates for either home.
- `bitrouter observe status` is an app command that reads a compile-time flag
  *out of* the crate.

Rename the concern and the sentence that started the whole chain becomes
unwritable. Nobody would document `MetricsRenderer` as *"the OSS binary uses
`bitrouter-telemetry`'s `PrometheusHook`"* — the name would not fit the claim.
That is the property a crate name buys, and #809 spends it: the module is now
`bitrouter_sdk::otel`, inside the crate that also owns `ObserveHook` and
`MetricsRenderer`, so the distinction between *the contract* and *one optional
renderer of it* has no structural marker at all.

**The split this document draws, stated once:**

- **Observability** is the contract — `ObserveHook`, `MetricsRenderer`,
  `SpanAttributes`, the span schema — plus what `apps/bitrouter` natively does
  with it. It is not optional and it is not a plugin.
- **Telemetry** is optional egress: rendering the contract onto a wire and
  shipping it somewhere. It always binds to the contract; it never defines it.

### 3. The public-dependency liability is permanent, and was conceded rather than answered

`crates/bitrouter-sdk/public-api-deps.txt` carries `tracing_core` and
`tracing_subscriber`. Its own header says why: *"`tracing_subscriber` and
`tracing_core` are on this list precisely because the tracing bridge's return
type and bound put them there, deliberately."* One function does it —
`otel::subscriber::tracing_subscriber_layer`.

`OTEL_TIERING_SPEC.md` D4 resolved that function's fate: **it stays.** The
blocking cloud grep returned hits, and cloud builds its own ingress `TraceLayer`
on purpose, so the bridge is load-bearing for the one out-of-tree consumer. D4
records the consequence in its own words: *"it stays, and its cost stays with
it… That is now a settled cost, not an open question."*

The cost, spelled out: `tracing_subscriber` 0.3 and `tracing_core` 0.1 are
**permanent public dependencies of the foundation crate**, both 0.x, where every
minor is a breaking release. A `tracing-subscriber` 0.4 forces a breaking
`bitrouter-sdk` release for every consumer in the workspace, including the five
that never enable `otel`.

`OTEL_SDK_MIGRATION_SPEC.md` D1 does not dispute this. It says SDK placement is
*"measurably worse on three axes, and the decision is to accept that, not to
deny it"* — semver, graph position, containment cost — and buys all three with
a single fact: cloud already takes `bitrouter-sdk`, so a standalone crate
*"would be a second published dependency delivering code the first one could
carry."*

That is one line of `Cargo.toml`, weighed against a permanent semver liability
on the crate every other crate depends on. Cloud takes two crates today. The
same section names its own revisit trigger — *"if `tracing-subscriber` 0.4
ships… the standalone crate becomes the better shape again"* — but that trigger
is written backwards: it fires when the bill arrives, not when the debt is
booked. D4 booked it.

### 4. Phase 0 changed the facts under the decision

Before `OTEL_TIERING_SPEC.md` phase 0, BitRouter's span schema existed only as
~90 `KeyValue::new` call sites inside `exporter.rs`, `acp.rs` and `metrics.rs`.
The exporter *was* the contract. That is why *"the schema must be identical
across deployments"* read as an argument for SDK placement — you cannot ship a
contract that only exists as one implementation's call sites.

Phase 0 landed. `crates/bitrouter-sdk/src/otel/schema.rs` declares every span,
attribute, event, metric and silent-failure invariant; its only `use` is
`serde::Serialize`; it names no `opentelemetry` type; it renders to the
committed `span-schema.json`; and two conformance tests drive real lifecycles
and fail on an undeclared key or a missing required one, verified
non-vacuous in both directions.

So the premise is spent. The schema can stay in the SDK while the renderer
leaves, and *"always bind to the observability contract"* stops being an
aspiration and becomes a diffable artifact with a conformance suite behind it.
`otel/mod.rs` already says as much about `schema.rs` — *"it could be lifted out
of the gate untouched should a consumer ever want the schema without the
feature"* — and then calls it *"a property, not a plan."* This document is the
plan.

## Decisions

### D1 — the OTLP renderer ships as its own crate

`bitrouter-sdk` keeps the contract. A new crate owns the renderer and depends
on the SDK, exactly as `bitrouter-observe` does today and exactly as
`bitrouter-guardrails` does for its own concern. The layering direction is
unchanged and remains one-directional.

This *reinstates* the shape `bitrouter-observe` had. What is different is not
the graph; it is that the contract it binds to now exists as a declared
artifact rather than as the renderer's own call sites, and that the crate is
named for one optional egress path rather than for the whole domain.

### D2 — the cut is the whole module minus the contract tier

**Not** `OTEL_TIERING_SPEC.md`'s phase 3, and the difference is the reason this
is executable where that was not.

| Stays in `bitrouter-sdk` | Moves to the new crate |
| --- | --- |
| `otel/schema.rs` (1,169 lines; ~986 production) | `exporter.rs`, `acp.rs`, `metrics.rs` |
| `otel/span_attributes.rs` (43 lines) | `config.rs`, `transport.rs`, `auth_client.rs`, `bearer.rs`, `processor_runtime.rs` |
| `span-schema.json` and its staleness test | `cardinality.rs`, `subscriber.rs`, `http_layer.rs` |
| `ObserveHook`, `MetricsRenderer`, `serve_with_router_wrapper` | the `otel` / `otel-http` / `otel-grpc` / `__otel-core` features and their whole closure |
| — | `tracing_core` and `tracing_subscriber` leave `public-api-deps.txt` |

Roughly **5,300 lines move; ~1,030 stay.** Both figures are raw file counts
including `#[cfg(test)]`, which is ~42% of the tree.

Two consequences worth stating, because they are what phase 3 could not
deliver:

- **The eager-tracer-binding hazard does not arise.** Phase 3 split *within*
  `otel/` — emission stays, export leaves — and hit it: `global::set_tracer_provider`
  has zero call sites in the workspace, `OtelExporter` deliberately builds a
  per-exporter provider, and a `BoxedTracer` obtained before a global provider
  is installed wraps a `NoopTracer` for its whole lifetime and drops every span
  silently. D2 moves emission and export **together**, which is what the crate
  already is. No new ordering constraint, and the global-provider question is
  not reopened.
- **The `tracer_clone` argument retires without being argued.**
  `OTEL_SDK_MIGRATION_SPEC.md` records that `acp.rs` could not move to the app
  because `AcpSpanRecorder` stores the exporter's tracer as a field, forcing
  `pub(crate) tracer_clone()` public across a boundary. Under D2 nothing
  crosses a boundary: `acp.rs` and `exporter.rs` stay in the same crate, as
  they are today.

**`SpanAttributes` stays on the contract side** and this is deliberate, not
incidental. Cloud imports it (`src/v1/settlement.rs`), the extension-region
enforcement point (`schema::is_reserved_attribute_key`) is already in
`schema.rs`, and the type is how a deployment adds attributes the contract does
not know about — which makes it contract vocabulary, not renderer machinery.
It names no `opentelemetry` type.

**`schema.rs` and `span_attributes.rs` come out from behind the feature gate.**
They carry no dependency beyond `serde`, and the reason they were gated —
*"that is where a reader looks for it"* — stops applying once the renderer is
elsewhere. Their new home is a `pub mod observe` (or `pub mod telemetry`) on
the SDK root, ungated, beside `metrics.rs`. This is the concrete form of "the
contract is not optional."

### D3 — the crate is named `bitrouter-telemetry`

**Taken, with a live objection that is recorded rather than waved past.**

The word is already load-bearing in this codebase at a **narrower** scope. All
of these mean *the first-party opt-in that exports to BitRouter's own
endpoint*, which is one endpoint choice inside the crate:

- `plugins.bitrouter-observe.telemetry` (`assemble.rs:1451`, `TelemetryOptIn`)
- `BITROUTER_TELEMETRY_TOKEN`
- `TelemetryLevel`, `TelemetryAttribution`, `TelemetryBearer`

Naming the crate `bitrouter-telemetry` makes the crate a superset of a word
that already denotes a subset of it.

**Amended: the collision is resolved rather than tolerated.** See D6. On
re-examination the narrowness lives in the YAML *sub-key*
(`plugins.*.telemetry` — a first-party endpoint preset), not in the env prefix:
`BITROUTER_TELEMETRY_{TOKEN, CONTENT_CAPTURE, CONTENT_ATTR_MAX_BYTES}` are all
one thing — auth for the export, what goes into it, and a size cap on it.

It is taken anyway, for one reason: the crate's role is *optional egress*, not
*OTLP*. `opentelemetry-rust` marks Traces-API, Traces-SDK and the OTLP trace
exporter all `Beta`; the `gen_ai.*` vocabulary has moved out of the main
semconv registry onto its own cadence with everything at `stability:
development`; OpenInference is a live competing vocabulary. Baking `otel` into
the crate name repeats `bitrouter-observe`'s error in the opposite direction —
too narrow instead of too broad — and a second renderer binding the same
`span-schema.json` would then have no home that fits.

**Conditions on taking it:**

1. The crate's module header states what the word covers here. **Met**, and
   simplified by D6 — with the config surface renamed, `telemetry` means one
   thing at the crate boundary and the sub-key `plugins.*.telemetry` is an
   ordinary preset inside it, not a competing scope.
2. ~~`plugins.bitrouter-observe.*` → `plugins.bitrouter-telemetry.*` is filed as
   its own issue with a deprecation window, **not bundled here**.~~
   **Superseded by D6, and the reasoning behind it was wrong.** It conflated
   *renaming the reader* (breaking) with *aliasing* (not breaking, ~8 lines at
   one call site) — with an alias there is no silent failure at all, so nothing
   forced a separate issue. What actually forced care was that the failure is
   silent, and D6 fixes that for every plugin key rather than aliasing this one.
3. ~~Until (2) lands, the crate reads a config key named for a different
   crate.~~ **Does not arise.** D6 renames it.

**The alternative is `bitrouter-otel`**, which the tiering spec used. It is
precise, collides with nothing, and is correct if the answer to "will there
ever be a second renderer?" is no. **What would select it:** a decision that
`span-schema.json` will only ever have one renderer, at which point the crate
should be named for it. Do not pick it merely to dodge a naming collision —
that trades a fixable inconsistency for a permanent mis-scoping, and D6 fixed
the inconsistency.

### D4 — redirect PR #809 in place; do not merge and revert

PR #809 is open, mergeable, and 50 files. Most of it is not placement: the span
schema artifact and its conformance tests, the OTel-native ingress span, the
`sdk-public-api` job and its pins and sentinels, `docs/CLI.md`'s log-target
contract, and the `main` merge resolution are all wanted under either shape.
What changes is the destination directory and the manifest.

Merging first and reverting later is strictly worse on the only cost that
matters here — see *The window*.

### D5 — what is frozen, and what only looked frozen

**Amended.** The original read: *everything* previously frozen stays frozen —
`plugins.bitrouter-observe.*`, `BITROUTER_OBSERVE_*`, the `/metrics` migration
banner, the `io.bitrouter.observe` scope, the `bitrouter` meter name, and the
pinned log targets. That list conflated two kinds of string, and D6 splits it.

**Frozen permanently, and not for tidiness reasons:**

| String | Why it can never move |
| --- | --- |
| `io.bitrouter.observe` | The OTLP instrumentation scope. Downstream dashboards and collector routing rules select on it. It is a protocol constant that happens to look like a name. |
| `bitrouter` (meter name) | Same: wire contract. |
| `bitrouter::observe::http` · `::cardinality` · `::span_attributes` | `RUST_LOG` has no validation and never can, so a rename is silently wrong for every operator holding a selector, with **no safety net available at any cost**. Worse: these were pinned *specifically* to survive crate moves — renaming them on a crate move defeats the pin's only purpose. |
| The `/metrics` migration banner's **first line** (`# Prometheus metrics have been removed…`) | Operator-facing text a deployment may be matching on. Its **second line** names the config key and therefore tracks it — freezing an instruction that has become wrong is not compatibility, it is misdirection. `apps/bitrouter/tests/e2e.rs` already asserted the two lines separately, which is what made the split obvious. |

**Renamed by D6** — these were frozen because the failure was silent, not
because they were contract:

| From | To |
| --- | --- |
| `plugins.bitrouter-observe.*` | `plugins.bitrouter-telemetry.*` |
| `BITROUTER_OBSERVE_CONTENT_CAPTURE` | `BITROUTER_TELEMETRY_CONTENT_CAPTURE` |
| `BITROUTER_OBSERVE_CONTENT_ATTR_MAX_BYTES` | `BITROUTER_TELEMETRY_CONTENT_ATTR_MAX_BYTES` |

**The rule the split produces, which is the point of doing it at all:**

> **`telemetry` is what an operator configures. `observe` is what BitRouter
> promises and how it identifies itself on the wire.**

That is a real line, not a historical accident, and it is why the result is a
stable end state rather than a half-finished rename. `bitrouter_sdk::observe`
sits on the `observe` side because it is the contract, and the contract
genuinely is observability.

"Frozen" still means the **strings**. `OTEL_TIERING_SPEC.md` phase 2 already
changed what `bitrouter::observe::http` carries and what suppressing it does;
that stands, and `docs/CLI.md` documents it.

### D6 — rename the config surface, behind a guard that makes it loud

The config key had exactly the defect the crate had. Everything under
`plugins.bitrouter-observe` is **egress** configuration — `.telemetry` (a
first-party endpoint preset), `.otel` (endpoint, headers, sampler, batch,
cardinality, bearer), the legacy `.otlp_endpoint`, and the two content-capture
env vars. Nothing under it configures observation; the binary's own observation
plane (`metering/`, `trajectory/`, `adequacy/`) is configured elsewhere
entirely. D1 fixed the code and left the operator-facing name wrong.

**The long-run argument is this document's own lead argument, turned on
itself.** *Why* 1 says a boundary defended in prose in three files is one the
code has stopped expressing. Keeping the old key requires exactly that, and the
prose already exists — `assemble.rs` carries a standing *"`bitrouter-observe`
is a stable historical config name… **Do not tidy it to match**"*. That comment
would have to live forever, and grow a twin wherever else someone trips over
the mismatch. The rename deletes it.

**Alpha is not the justification, and it is important not to let it be.** A
pre-1.0 version removes the *deprecation window* — no alias, one changelog
entry. It does nothing about the *silent* failure: an operator upgrading
`alpha.27` → `alpha.28` gets `exporter_wired: false` and dark dashboards with
no error anywhere, and semver license does not help them debug that.

#### The guard is the load-bearing half, and it is not about telemetry

`config.plugins` is an unvalidated `HashMap<String, Value>`, so it swallows
**every** unknown key. That is a live defect today, independent of any rename
and sharper elsewhere:

```yaml
plugins:
  bitrouter-guardrail:      # typo — singular
    custom_patterns: [...]
```

`assemble.rs` falls through to `GuardrailConfig::default()` and the operator's
declared block/redact patterns silently never apply. `dist/schema/bitrouter.config.schema.json`
does not catch it either — `plugins` is `additionalProperties: true`, as
permissive as the map.

So D6 ships an unknown-key warning **first**, at `bitrouter config validate`
*and* at daemon startup — startup matters more, since `validate` is opt-in and
the daemon always runs. That converts this class of failure from silent to
loud for every plugin, and it is what makes the rename boring rather than
risky. It is worth doing whether or not the rename ever happens.

Environment variables are the worst case on the board — env has no validation,
no schema, and no possible one — but the obvious remedy does **not** work, and
that is a finding rather than a deferral. A blanket "unrecognised `BITROUTER_*`"
scan would fire on the operator's own variables: `${VAR}` substitution lets a
config reference any name, so `api_key: ${BITROUTER_MY_KEY}` is legitimate and
unknowable to the scanner. D6 names the two renames exactly instead. That is
precise, has no false positives, and is what an operator hitting this actually
needs told — at the cost of being a migration aid rather than a standing guard.

#### The env rename does NOT universally fail closed

**This section originally claimed it did. That was wrong, and the counterexample
is the one that matters.**

The claim held for the case it was reasoned about: when the environment is the
*only* source, `ContentCaptureMode` defaults to `Off` and the cap to 128 KiB, so
a stale name exports less content and truncates more.

It fails **open** when the environment was overriding YAML *downward*.
`OtelConfig::with_env_overrides` assigns unconditionally, and `content_capture`
is also settable from YAML — directly, or via `telemetry.level: full`. So an
operator whose config says `full` and who pinned
`BITROUTER_OBSERVE_CONTENT_CAPTURE=off` in a service unit as a kill-switch gets
the opposite of what they pinned: the stale name is ignored, YAML wins, and
prompt and response bodies begin leaving the process. The same shape applies to
a cap the environment had lowered.

This is why the renamed-variable check is a *named* check that fires on the old
name rather than a general "unrecognised variable" scan, and why the warning is
not treated as a nicety. It is also the strongest argument for keeping the
`BITROUTER_OBSERVE_*` names on the deprecation list for longer than the YAML
key, should that ever be revisited.

The YAML key does fail closed — no exporter is wired, so nothing leaves — but
that is an observability outage rather than a safety problem, which is what the
startup warning is for.

#### As built

Five things worth recording, one of which is a defect this document's own
sequencing did not prevent.

- **The guard was silent on exactly the path it exists for, in its first
  form.** It logged from `build_app_with_path` — which runs at
  `main.rs:2663`, five lines *before* `init_serve_tracing_subscriber` at
  `:2668`. Every warning went into a process with no subscriber installed and
  was dropped. The surrounding code already documents this hazard and works
  around it for `otel_init_error` (*"a tracing line here would be dropped"*),
  and it was walked into anyway. Caught by running the daemon against a stale
  config and finding an empty log, not by any test. Now collected in assembly
  and carried out as `Assembled::ignored_config`, emitted by the binary after
  the subscriber init; `tests/daemon.rs` pins the deferral so a future edit
  cannot quietly move it back.
- **The env half is pure so it can be tested.** `renamed_env_warnings` takes
  the presence check as a closure rather than reading the environment —
  process-global state would make the test order-dependent inside a shared test
  binary, the same hazard `ingress_log_target.rs` is a separate binary for. It
  also settles a case the plan did not state: when *both* names are set, nothing
  is warned. The operator has migrated and left a stale export behind, which is
  not worth a line on every start.
- **The `/metrics` banner splits in two**, and `apps/bitrouter/tests/e2e.rs`
  had already anticipated it — two assertions, one on `Prometheus metrics have
  been removed` and one on the config key. D5's freeze applies to the first;
  the second names the key and must track it, because freezing an instruction
  that has become wrong is misdirection, not compatibility.
- **`config validate` reports, never fails.** `ignored_config` is its own
  field rather than another `warnings` entry (different shape, and a consumer
  already parses `warnings[].unset_env`), and `valid` is untouched: an ignored
  block is a misconfiguration, not a malformed config, and this command gates
  CI.
- **Verified live**, not only by test: a daemon started against a config
  carrying both `plugins.bitrouter-observe` and a typo'd
  `plugins.bitrouter-guardrail`, with `BITROUTER_OBSERVE_CONTENT_CAPTURE` set,
  emits all three warnings after subscriber init and serves normally.

#### The legacy `otlp_endpoint` shim goes with it

`plugins.bitrouter-observe.otlp_endpoint` is a v0 carry-over already marked
*"will be removed in v1.1"*. It is removed here instead, and not merely to save
a release: keeping it would mean carrying a **v0** compatibility path under a
key name v0 never had. `plugins.bitrouter-telemetry.otlp_endpoint` is a
combination that has never existed in any released config, so preserving it
would be preserving nothing. One breaking change to communicate, not two.

## The window

**Cloud has not migrated.** `OTEL_TIERING_SPEC.md` D4 read `bitrouter-cloud` at
`origin/main` `e224842f` (v0.23.0) and found it still pinning
`bitrouter-observe = { version = "1.0.0-alpha.27", features = ["otel-http"] }`
from crates.io, importing `otel::http_layer::tracing_subscriber_layer`,
`otel::{MetricsConfig, OtelConfig, OtelExporter, OtelObserveHook, SamplerKind}`
and `otel::SpanAttributes`. It builds only because the crates.io index is
append-only. It owes a migration that predates this document.

That makes the cost asymmetric and time-boxed:

| | cloud migrations |
| --- | --- |
| decide now, redirect #809 | **one** — `bitrouter_observe::otel::` → `bitrouter_telemetry::otel::` |
| merge #809, decide later | **two** — first to `bitrouter_sdk::otel::`, then again |

Both migrations are the same mechanical shape (three files, seven items, one
module move for `tracing_subscriber_layer`: `http_layer` → `subscriber`). The
question is only how many times cloud does it. **The window closes when #809
merges**, and nothing else about this decision is urgent.

## Evidence

Verified in-tree at `pr809` unless marked otherwise.

| Claim | How verified |
| --- | --- |
| `tracing_core` + `tracing_subscriber` are public deps, caused by the bridge | `public-api-deps.txt` contents and its own header |
| `schema.rs` names no `opentelemetry` type and depends only on `serde` | its sole `use` line; the two `opentelemetry` occurrences are prose |
| the app registers a native `ObserveHook` beside the OTel one | `assemble.rs:649-651`, `response_observer.rs:206` |
| the app owns its `MetricsRenderer` | `assemble.rs:1219` |
| `serve_with_router_wrapper` exists for an external observe crate | `server.rs:68` — *"used by `bitrouter-observe`"* |
| "telemetry" is already scoped to the first-party opt-in | `assemble.rs:1451` `TelemetryOptIn`, `BITROUTER_TELEMETRY_TOKEN` |
| three doc sites defend the boundary in prose | `sdk/lib.rs`, `sdk/metrics.rs`, `docs/DEVELOPMENT.md`, quoted above |
| the `sdk-public-api` job pins a `subscriber`-module sentinel | `.github/workflows/ci.yml` — `'pub fn bitrouter_sdk::otel::subscriber::tracing_subscriber_layer'` |
| per-file line counts and the ~42% test fraction | `wc -l` per file; first `#[cfg(test)]` offset per file |

**Inherited, not re-verified.** Cited from `OTEL_TIERING_SPEC.md` rather than
re-run, and a reviewer should treat them as that document's evidence:

- the cloud grep at `e224842f` and everything in *The window* that rests on it;
- the build-cache timings and the 159/201 crate counts;
- the eager-tracer-binding read of `opentelemetry-0.32.0/src/global/trace.rs`.

**Not verified — the open risks:**

- **Nothing here proves the emitted span tree survives the move.** It should —
  the code moves whole and the conformance tests move with it — but "should" is
  what the acceptance gate is for. The gate is three tests, not one; see below.
- **The `public-api-deps.txt` delta is predicted, not measured.** The claim is
  that `tracing_core` and `tracing_subscriber` leave. It holds only if nothing
  else public needs them, which the file's header asserts and this document has
  not independently confirmed. **Run the pinned job before quoting the delta as
  a result.**
- **`http_layer.rs` after phase 2 needs `dep:tower` and axum in the new crate.**
  `bitrouter-observe` already carried axum, so this is a restoration rather
  than a new cost — but the `feature-isolation` job's shape changes, see below.

## Constraints

### The `sdk-public-api` sentinel must be swapped in the same commit

`ci.yml` proves its listing is non-vacuous with two literal sentinels, one of
which is `'pub fn bitrouter_sdk::otel::subscriber::tracing_subscriber_layer'`.
Both sentinels name `bitrouter_sdk::otel`. Move the module and **both** go
missing — the job then fails loudly, which is the good case, but a "fix" that
merely deletes them leaves every negative gate passing vacuously over an
empty-ish listing. That is the exact silent pass the job exists to prevent.

Replace with two sentinels over surface that remains: the contract module and a
proven-public item inside it. `OTEL_TIERING_SPEC.md` D4 flagged this same trap
for its removal branch; it applies here unchanged.

**The job itself stays.** Its `no opentelemetry type in the SDK's public API`
and `no re-export of an OTel type` gates become trivially true, and that is
fine — they are the guard that keeps them true after the next person adds a
convenience re-export. The re-export gate in particular caught a real blind
spot (`cargo public-api` renders a bare re-export under its *local* path,
hiding the origin crate) and should not be deleted as redundant.

### `feature-isolation` changes shape and must not be weakened

Today the job asserts, positively, that `axum` and `opentelemetry` resolve in
`bitrouter-sdk --all-features` — and exits with *"misspelled in this list?"* if
they do not. After the move, `opentelemetry` is absent from that tree by
design, so the positive assertion must move to the new crate rather than be
deleted. The negative loop (`bitrouter-guardrails`, `bitrouter-providers`,
`dist-helper` must not reach `axum` or `opentelemetry` when resolved alone)
gains the new crate as a subject for `axum` only.

The `__otel-core` transport-less `compile_error!` step and the per-transport
`cargo check` steps move with the features they guard. None of them is
retired.

### The acceptance gate is three tests, not one

Inherited verbatim from `OTEL_TIERING_SPEC.md`, and it is the trap in this
change too. `observe_hierarchy.rs` does **not** assert the full hierarchy:

| Shape | Where |
| --- | --- |
| SERVER → root `chat`, scope name/version, matching trace ids | `apps/bitrouter/tests/observe_hierarchy.rs` |
| `route` / `settle` parenting on root `chat` | `apps/bitrouter/tests/full_stack.rs` — no SERVER span at all |
| `route` / `settle` parenting, in-crate | `exporter.rs`'s conformance tests |

Plus `crates/bitrouter-sdk/tests/ingress_log_target.rs`, which must move to the
new crate and stay its own test binary — `tracing`'s per-callsite `Interest`
cache is process-global, and merging it into a shared binary makes it pass or
fail depending on test ordering.

### The contract tier must not acquire a dependency on the way out

`schema.rs` and `span_attributes.rs` leave the feature gate. If either picks up
anything beyond `serde` in the move, the move is wrong: an ungated module in
the foundation crate is the strictest position in the workspace. The
`span-schema.json` staleness test moves with them and must run under the SDK's
**default** features afterwards, not `--all-features`.

## Cost

Stated plainly, because the tiering spec's *Cost* section was written after its
decision and this one should not be.

- **5,594 lines relocate** (against ~5,300 estimated), for the second time in
  one release cycle, having just relocated in #810. 1,275 stay as the contract.
- **PR #809 is redirected, not merged.** Its schema artifact, ingress span,
  CI job and docs survive; its directory and manifest do not. Expect the diff
  to stay near 50 files.
- **Doctrine text is rewritten a second time** — `sdk/lib.rs`,
  `sdk/metrics.rs`, `docs/DEVELOPMENT.md` — reverting to a two-crate framing
  and deleting the three defensive paragraphs quoted in *Why* 1.
- **Three spec documents end up describing one decision.** This one supersedes
  `OTEL_SDK_MIGRATION_SPEC.md` D1 and reopens `OTEL_TIERING_SPEC.md` D1 on
  grounds that document reserved. Neither is deleted; both are marked, because
  the reasoning in them is what stops the next reader relitigating this from
  scratch.
- **Cloud does one migration** (see *The window*) — the one it already owes.
- **`bitrouter-cloud` takes two published dependencies instead of one.** This
  is the entire cost `OTEL_SDK_MIGRATION_SPEC.md` D1 paid its three conceded
  axes to avoid, and it is one line of `Cargo.toml`.

What is **not** claimed as a benefit: a lighter build, fewer crates, faster CI.
See *The arguments that are dead*.

## Migration

| Phase | Change | Breaking? | Gate |
| --- | --- | --- | --- |
| **1** | Lift `schema.rs` + `span_attributes.rs` + `span-schema.json` out of the `otel` gate into an ungated `observe` module on the SDK root — **done** | no | met: staleness test green under **default** features, once a pre-existing gated-import defect was fixed (see *As built*); no new dependency |
| **2** | `git mv` the rest of `otel/` into `bitrouter-telemetry`; repoint `apps/bitrouter`; swap the `ci.yml` sentinels; move `ingress_log_target.rs`; rework `feature-isolation` — **done** | **yes** for cloud | met: all four tests green; `public-api-deps.txt` lost exactly two lines |
| **3** | Rewrite the doctrine text; mark both predecessor specs; update `docs/DEVELOPMENT.md`'s crate table and `release-plz.toml` — **done** | no | met: lockstep list below, minus `.github/copilot-instructions.md` — see its note there |
| **4** | D6's guard: unknown `plugins.*` keys reported at `config validate` and at daemon startup, plus a named check for the renamed env vars — **done** | no | met: a typo'd `bitrouter-guardrail` is reported, and the deferral that makes it visible on `serve` is pinned by a test |
| **5** | D6's rename: `plugins.bitrouter-observe.*` → `plugins.bitrouter-telemetry.*`, `BITROUTER_OBSERVE_*` → `BITROUTER_TELEMETRY_*`, legacy `otlp_endpoint` removed. No alias — pre-1.0 — **done** | **yes**, and loudly, because phase 4 shipped first | met: the three frozen names in D5 are untouched; 2,872 tests pass |

Phase 1 lands alone and is worth having regardless — an ungated, dependency-free
contract module is correct under #809's shape too. **If this document is
rejected, land phase 1 anyway.**

Phases 2 and 3 ship together: a tree with the code moved and the doctrine still
claiming it lives in the SDK is worse than either end state.


## As built

Six things the plan did not have right, or did not have at all. None changed a
decision; all six are things the next reader would otherwise rediscover.

- **The contract tier has a name, and it is `observe`.** The spec said "a
  `pub mod observe` (or `pub mod telemetry`)" and left it open. `observe` won
  because the frozen strings already use it — `io.bitrouter.observe` is the
  instrumentation scope, `bitrouter::observe::*` are the pinned log targets — so
  the module name, the scope and the targets now say the same word for the same
  thing. It also states the split in the tree itself: `bitrouter_sdk::observe`
  is the contract, `bitrouter-telemetry` is one egress path.

- **The schema had to become genuinely public, and that is a real API
  expansion.** Every item in `schema.rs` was `pub(crate)`, and three helpers —
  `span_def_for`, `value_type_matches`, `literal_prefix` — were `#[cfg(test)]`,
  which cost nothing while the only conformance suite lived in the same crate.
  It does not any more: **a check a second renderer cannot call is not a
  contract.** All of it is now `pub`, with docs, and the `#[cfg(test)]` gates
  are gone. This was the change most likely to move `public-api-deps.txt`, and
  it did not — the schema is plain data over `serde`, and `serde_core` was
  already on the list.

- **The telemetry crate needs a `server` feature.** The spec listed
  `http_layer.rs` as moving without saying what gates it. It is gated `server`,
  which pulls `axum`, `http-body`, `pin-project-lite` and `bitrouter-sdk/server`.
  Folding it into `__otel-core` would have been simpler and wrong: the known
  out-of-tree consumer wants the bridge (`otel::subscriber`) and builds its own
  ingress span on purpose, so it would have compiled axum for nothing.

- **A pre-existing defect had to be fixed for acceptance to be checkable.**
  `language_model/context.rs`'s test module imported `PromptOverrides` through
  `crate::config`, a `config_file`-gated re-export, so `bitrouter-sdk`'s
  lib-test target did not build under default features at all. The SDK's own
  bench manifest documented this as a known workaround rather than fixing it.
  It blocks the span-schema staleness test, which must run under default
  features now that `observe` is ungated. Repointed to the canonical
  `crate::language_model::routing::PromptOverrides`; no behaviour change.

- **`feature-isolation`'s "SDK default tree stays lean" list collapses to one
  name.** It asserted eight; seven were OTel names that moved. The eighth,
  `axum`, is the only one left — and the obvious addition, `tower`, is **not a
  valid subject**: `reqwest` puts it in the SDK's default tree regardless of the
  `server` feature. The old list never had to notice, because the OTel names
  carried the step.

- **The job gained a strictly stronger gate than the one it replaced.** The old
  assertion was "the OTel stack is not in the SDK's *default* tree". The new one
  is "no `opentelemetry*` crate is in `bitrouter-sdk --all-features`" — absence
  at every feature combination, not absence by default. A change that quietly
  pulled a renderer back into the foundation crate would have passed the old
  check and fails this one. `sdk-public-api`'s `opentelemetry` grep is
  correspondingly demoted from live guard to forward guard, and its comment says
  so.

### What the first CI run found

Both guard jobs failed on the first push, and neither failure was a defect in
the move. Recorded because both are instructive about the guards themselves.

- **`feature-isolation`'s OTel-absence list was over-broad.** It named
  `tracing-subscriber`, `tonic` and `dashmap` alongside the `opentelemetry*`
  crates. Those three are general-purpose, and `tracing-subscriber` promptly
  proved it: `main`'s `agent-client-protocol-conductor` pulls it, so the step
  failed on a dependency that has nothing to do with telemetry. **The error was
  conceptual, not clerical** — the semver argument this whole document rests on
  is about *public* dependencies, which `public-api-deps.txt` guards. Tree
  membership costs nothing. The step is now scoped to the five crates that can
  only be there because of the renderer, with the distinction written into the
  comment so it is not re-widened.

- **`public-api-deps.txt` was regenerated before the merge with `main`, and
  went stale without saying so.** The file is rewritten wholesale, so git
  resolved it without a conflict and simply kept this branch's copy. That is a
  general hazard for any generated file, and its header now says: regenerate
  *after* merging, never before.

  Re-running it post-merge is what makes the headline claim trustworthy rather
  than lucky: **`tracing_core` and `tracing_subscriber` are still gone.** The
  only addition is `agent_client_protocol`, and it is not this change's —
  `main`'s `acp::controller::Controller` names it in a generic bound and in an
  `impl ConnectTo for acp::up::AgentProcess`.

  **That is the guard catching exactly what it was built for, on its first
  run.** `public-api-deps.txt` does not exist on `main`; the job arrived with
  #809 and lives only on this branch. So #849 added a public dependency to the
  foundation crate with nothing in place to notice, and the first thing this
  job ever did was notice. The manifest records where the line came from.

- **A pre-existing violation surfaced and was then fixed, not carved out.**
  `dist-helper` reached `axum`: it enabled `bitrouter-sdk/acp`, which pulled
  `agent-client-protocol-conductor`, which pulls
  `agent-client-protocol-trace-viewer`, which depends on `axum`
  non-optionally. `main`'s version of `feature-isolation` had no such loop — it
  still ran `cargo check -p bitrouter-observe` and asserted one thing — so it
  was never checked there.

  It was briefly carved out of the job, on the strength of a claim that turned
  out to be **wrong**: that trimming `dist-helper`'s features would gut
  `dist/schema/bitrouter.config.schema.json`. That came from an experiment
  which dropped `mcp` *and* `acp` and was judged from a truncated diff. Dropping
  `acp` alone leaves the schema semantically identical — `mcp` is the one the
  schema needs, and `dist-helper` never used `acp` at all.

  **The fix that shipped is one part, not two.** `dist-helper` stops enabling a
  feature it never used — that removes the carve-out, puts `dist-helper` back in
  the main loop, and clears both `axum` and `opentelemetry` from its tree.

  A second part was built and then **withdrawn**: moving the conductor behind a
  new `acp-controller` feature so `acp` would stop dragging an HTTP server. It
  died on this document's own standard. *Name the beneficiary:* `apps/bitrouter`
  is the only consumer of `acp` in the workspace and it wants the controller, so
  after the `dist-helper` trim the split helped **nobody**. It also stopped being
  cheap: `main`'s #848 fused the halves — `acp::client` imports `controller`'s
  route-control types — so a narrow gate on `Controller::run` left half the
  module unreachable, which `-D warnings` correctly rejected as dead code, and
  the honest version would have meant restructuring a module that landed the day
  before. The real fix is upstream making
  `agent-client-protocol-trace-viewer`'s `axum` dependency optional.

  Recorded rather than quietly dropped, because the invariant is still worth
  wanting: `docs/DEVELOPMENT.md` states it as a known wart with the condition
  for revisiting it.

  It also exposed something worse than the axum edge, which is recorded under
  *Measured*: the committed schema's **byte order** depended on that dependency
  chain.

### What an adversarial review found

An independent review of the branch found five things this document had wrong.
All are fixed; they are recorded because four of them are failures of the same
kind — a claim checked by reasoning where it should have been checked by
running something.

- **`KNOWN_PLUGIN_IDS` was missing `bitrouter-policy`, and the test could not
  have caught it.** There are three readers, not two: `load_policy_store` gets
  its block through a line-wrapped `config` / `.plugins` / `.get(…)` chain that
  a single-line grep does not match. The consequence is the exact case the
  guard's own comment calls worse than no guard — every daemon start warning
  about a plugin it honours. `every_known_plugin_id_is_recognised` passed for
  any list including the empty one. Replaced by
  `known_plugin_ids_cover_every_reader`, which scans this file's own source for
  `.plugins` … `.get("<literal>")` and asserts the list matches in **both**
  directions; verified non-vacuous by deleting an id (fails) and by adding a
  phantom one (fails).

- **The guard never fired on the ACP path.** `bitrouter acp serve|prompt` takes
  its exporter from `build_otel_exporter_standalone_with_credentials` and never
  builds an `App`, so the one scenario D6 exists for — a stale key, no
  exporter, no error — stayed completely silent on a surface the skill
  documents as honouring the same telemetry config. Now emitted at the top of
  `acp_cli::build_observability`, where the subscriber is already installed.

  **The first fix missed half the surface.** `build_observability` is reached
  only by `chat`, `chat_piped` and `prompt`; `acp_cli::serve` — behind both
  `bitrouter acp serve` and `bitrouter spawn --serve` — never calls it, so the
  path the doc comment named by name stayed exactly as silent as before. It now
  emits the same set itself, first thing, on the config as read. Pinned by
  `tests/acp.rs::serve_warns_about_ignored_plugin_blocks`, which spawns
  `acp serve --direct` with an unknown agent id: routing short-circuits and the
  process exits right after the warnings, so no harness or credentials are
  needed to observe them.

- **The guard was blind one level down, on the natural migration path.** The
  id-level warning tells an operator to rename the block and nothing else. Doing
  exactly that while carrying a v0 `otlp_endpoint` sub-key along produced a
  known id, no warning, and no exporter. `REMOVED_PLUGIN_SUBKEYS` now names it,
  in the same shape as `RENAMED_ENV_VARS`.

- **"Fails closed" was false in the override-down direction.** See D6.

- **`config validate` and the daemon reported different sets.** Validate carried
  only unknown ids while the daemon also reported dead sub-keys. Split into
  `ignored_config_file_warnings` (what a file can tell you — used by validate,
  since it may be validating another machine's config) and
  `ignored_config_warnings` (that plus the environment, used by every runtime
  surface). The report field is `ignored_config` rather than
  `unknown_plugins`, matching what it now carries.

Two smaller ones: `build_otel_exporter_standalone` had no callers and is
deleted (CLAUDE.md rule 4), and `--features server` without a transport was a
silent no-op that still pulled axum — now a `compile_error!` mirroring the
transport guard.

**One claim in this document was retracted rather than fixed.** It said
`tests/daemon.rs` "pins the deferral so a future edit cannot quietly move it
back." It does not: it asserts the warnings are carried out, and re-adding a
`tracing::warn!` inside assembly would still pass. The stronger assertion —
that nothing was logged *during* assembly — was considered and rejected as
unreliable in the wrong direction: `tracing`'s per-callsite `Interest` cache is
process-global, so a sibling test touching the callsite with no subscriber
installed makes a "nothing was logged" assertion pass vacuously. The test's own
doc now says exactly this.

**One design objection is not resolved and is left open**, because it is a
judgement call rather than a defect: `observe::schema`'s `span_def_for`,
`value_type_matches` and `render_json` have no non-test callers anywhere —
their only users are conformance tests in `bitrouter-telemetry` and the
artifact staleness test here. Ungating them was justified by "a check a second
renderer cannot call is not a contract", which collides with this document's
own position that a second renderer is undecided. The alternative is a
`testing` feature. Recorded so the next reader can take it up deliberately.

### Measured

| | |
| --- | --- |
| `public-api-deps.txt` delta | **−`tracing_core`, −`tracing_subscriber`** — confirmed again after merging `main`. `+agent_client_protocol` is `main`'s, not this change's; see above. |
| `cargo tree -p bitrouter-sdk --all-features -i opentelemetry` | fails — absent, as do the other six OTel names |
| Lines moved / stayed | 5,594 / 1,275 (raw, ~42% `#[cfg(test)]`) |
| Tests | 2,893 passed, 0 failed, 11 skipped after merging `main` and applying the review fixes |
| Guard, live | A config carrying `bitrouter-policy` (read), `bitrouter-guardrail` (typo) and `bitrouter-telemetry.otlp_endpoint` (dead sub-key) reports exactly the latter two, and `bitrouter-policy` is no longer falsely flagged |
| `dist-helper` | reaches neither `axum` nor `opentelemetry`, and is back in `feature-isolation`'s main consumer loop. `acp` still links `axum` through the conductor — a known wart, see *As built*. |
| `dist/schema/…json` | now key-sorted by construction, and **byte-identical with and without** the `serde_json/preserve_order`-bearing chain — verified by re-adding `acp` to `dist-helper` and regenerating. Previously its byte order was a function of unrelated crates' feature unification: dropping one feature reordered all 3,234 lines without changing a value. |
| `cargo clippy --workspace --all-features --tests --benches` | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features` | clean |
| `cargo fmt --all -- --check` | clean |

One number is recorded as a **fact, not a justification**: 33 crates are in the
renderer's `--all-features` tree and not in the SDK's. *The arguments that are
dead* still applies — nobody was paying for them, `default = []` saw to that,
and this figure must not be cited as a benefit. It is here so the next person
measuring does not have to.

## Acceptance

1. **Met.** `apps/bitrouter/tests/observe_hierarchy.rs` — assertion block diffs
   to nothing; only import paths changed. SERVER span present, root `chat`
   parented on it, matching trace ids, scope name `io.bitrouter.observe`, scope
   version all assert as before.
2. **Met.** `full_stack.rs` and the conformance tests — now in
   `bitrouter-telemetry` — are green, including the three
   `*_conforms_to_the_committed_span_schema` cases and the two
   `SpanAttributes` reserved-region cases.
3. **Met.** `ingress_log_target.rs` moved to `bitrouter-telemetry/tests/` and is
   still its own binary, with `required-features = ["otel-http", "server"]`.
   Its `#![cfg(...)]` guard and the `Interest`-cache reasoning are unchanged.
4. **Met exactly.** `public-api-deps.txt` lost `tracing_core` and
   `tracing_subscriber` and nothing else, on `nightly-2026-05-05` /
   `cargo-public-api 0.52.0`. The file's header now records why they were there
   and why they must not come back quietly.
5. **Met.** Sentinels are `pub mod bitrouter_sdk::observe` and
   `pub fn bitrouter_sdk::observe::schema::is_reserved_attribute_key`, both
   confirmed present in the rendered listing. They are ungated, so unlike their
   predecessors they cannot go missing because a feature stopped resolving —
   their absence means the listing itself is broken, and the step now says that.
6. **Met.** `-i opentelemetry` fails against `bitrouter-sdk --all-features`, as
   do `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry`,
   `tracing-subscriber`, `tonic` and `dashmap`. This is now a CI step.
7. **Met.** All seven resolve in `bitrouter-telemetry --all-features`; none is in
   its default tree; `axum` and `tonic` are absent from the `otel-http` tree.
8. **Met.** `bitrouter observe status` is unchanged: `OTEL_ENABLED` moved crate
   but keeps its name, value and meaning, and `ObserveStatusReport` is untouched.
9. **Met.** 2,866 passed / 0 failed / 11 skipped; clippy, `cargo doc` under
   `-D warnings`, and `cargo fmt --check` all clean.

## Abandon triggers

**None fired.** #809 had not merged; acceptance 4 came back at exactly two
lines; phase 1 needed no new dependency; and no product decision ruled out a
second renderer. Kept below unchanged, because they are the conditions under
which this decision should be reversed in turn.

Stop and keep #809 as it stands if:

- **#809 merges before this is decided.** The window argument is the load-bearing
  one and it does not survive the merge; from that point the cost is two cloud
  migrations and the case has to be remade on semver alone.
- **Acceptance 4 comes back larger than two lines.** If something else public
  depends on `tracing_subscriber`, the semver argument in *Why* 3 loses most of
  its force and what remains is naming — which is real but is not worth a
  four-figure-line relocation on its own.
- **Phase 1 cannot be done without a new dependency.** If the contract tier
  cannot stand ungated and dependency-free, then it is not a contract tier, and
  *Why* 4's premise is wrong.
- **A second `ObserveHook` renderer is ruled out as a matter of product
  direction.** Then D3 resolves to `bitrouter-otel` and the crate is named for
  its one renderer — which weakens, but does not defeat, the rest of the
  document.

Note which trigger is **not** listed: cloud's willingness. `OTEL_TIERING_SPEC.md`
established the owner was willing to migrate, and cloud's migration is owed
either way.

## Lockstep (CLAUDE.md)

`crates/bitrouter-sdk/src/lib.rs` ("What ships here, and what ships elsewhere")
· `crates/bitrouter-sdk/src/metrics.rs` · `crates/bitrouter-sdk/src/server.rs`
(the `serve_with_router_wrapper` doc names the crate) ·
`crates/bitrouter-sdk/src/language_model/pipeline.rs:1138` ·
`docs/DEVELOPMENT.md` (crate table, "Dependency Logic" item 3, SDK feature
table) · `docs/CLI.md` (log targets) · `docs/README.md` (spec index) ·
`.github/workflows/ci.yml` (`feature-isolation`, `sdk-public-api`) ·
`release-plz.toml` (`changelog_include`) · `Cargo.toml`
(`[workspace.dependencies]`) · `.github/copilot-instructions.md` · **`bitrouter-cloud`**.

`.github/copilot-instructions.md` needed no change *from this work* — #809 had
already removed its `bitrouter-observe` mention — but it was gutted separately,
and the reason belongs on this document's record because it is the same failure
mode this spec is about. It was a second copy of material that already lived in
`AGENTS.md` and `docs/DEVELOPMENT.md`, nothing kept the copy in sync, and it
rotted: deleted crates, the wrong HTTP framework, a runtime path that never
existed. It is now a pointer with no facts of its own, which is the only shape
that cannot go stale. Note also that **`CLAUDE.md` never listed it** — that
claim came from issue #808's lockstep list, not from `CLAUDE.md` itself, and it
is retained in the list above only because a future editor will look for it.

No registry changes, no `dist/` rebuild.

`skills/bitrouter/` **is** affected and was updated: two env vars were renamed
(`BITROUTER_OBSERVE_*` → `BITROUTER_TELEMETRY_*`), the config key moved, and
`config validate` gained a reported field. An earlier draft of this line claimed
the opposite — that no env var changed — which was simply false, and it survived
review only because the skill happens not to document those two variables. The
skill's `config validate` row and its ACP observability paragraph both carry the
new names.
