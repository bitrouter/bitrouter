# OTel tiering spec

Status: **phases 0, 1 and 2 landed and stand. D1 was reopened and reversed —
see [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).**

> **Read this first.** D1 ("tier the dependency") was withdrawn below on
> measured benefit, and those measurements stand: the crate-count saving has no
> beneficiary and the build-cache saving is roughly zero. **The reversal did not
> reuse either argument** — see *The cloud question*'s own closing sentence,
> which reserves reopening for "a positioning decision needing different
> evidence than this section gathered." That is what happened. The renderer
> moved to `crates/bitrouter-telemetry` on naming, semver and timing:
>
> - **Semver.** D4's keep branch made `tracing_subscriber` 0.3 and
>   `tracing_core` 0.1 *permanent* public dependencies of the foundation crate.
>   That is now measured, not predicted: `public-api-deps.txt` lost exactly
>   those two lines and nothing else when the renderer left.
> - **Phase 0 is what made it cheap.** A declared, conformance-checked schema
>   is a contract a second crate can bind to. Before it, the exporter *was* the
>   contract, which is precisely why "identical across deployments" read as an
>   argument for SDK placement.
> - **The cut is different from phase 3's.** Phase 3 split *within* `otel/` and
>   hit the eager-tracer-binding hazard this document documents at length. The
>   reversal moved emission and export together, so that hazard never arises
>   and the `tracer_clone` question stays retired.
>
> **Everything phases 0–2 built is unchanged and now lives across two crates:**
> the schema declaration and `span-schema.json` in
> `bitrouter_sdk::observe`, the OTel-native ingress span and its
> `ingress_log_target` test in `bitrouter-telemetry`. The three-test acceptance
> gate, the frozen strings, and the `Interest`-cache reasoning for the separate
> test binary all still apply verbatim.
>
> The one finding this document flags as outranking all of it — that
> `bitrouter-cloud` still depends on the deleted `bitrouter-observe` — is
> **still open**, and is what made the reversal cheap: cloud performs one
> migration either way.

Original status: complete. Phases 0, 1 and 2 landed; phases 3 and 4 are
withdrawn.
Follows
[`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md), which moved the
exporter into `bitrouter-sdk`.

| Decision | State |
| --- | --- |
| **D2** — span schema as a committed artifact | **done.** Declared, committed, conformance-checked; the `SpanAttributes` hole closed as an open extension region. See *Phase 0, as built*. |
| **D4** — `tracing_subscriber_layer` | **done: it stays.** The blocking cloud grep returned hits, so `tracing_subscriber` / `tracing_core` are permanent public dependencies. |
| **D3** — OTel-native ingress span | **done.** Built as an axum middleware; `tower-http` dropped; `RUST_LOG` can no longer suppress a span. See *Phase 2, as built*. |
| **D1** — tier the dependency | **withdrawn.** The cloud question is answered: option 3. See below. |
| **D5** — `ObserveHook` conformance suite | **withdrawn as a phase.** It was only a prerequisite under a D1 option that is no longer live. |

**The headline reversal: D1 does not ship, and the reason is not the one this
document was braced for.** The cloud question is closed — the owner was willing
to migrate, so availability was never the binding constraint. Benefit was. Both
of phase 3's justifications were measured and neither survived: the crate-count
saving has no beneficiary that exists, and the build-cache saving is roughly
zero for a real OTel release. The numbers are in *The cloud question*. What
phase 3 was actually for — a swappable renderer — was delivered by phase 0,
which gives a second implementation a specification instead of a file layout.

**One finding outranks all of it and is not this document's to fix:**
`bitrouter-cloud` `origin/main` still depends on `bitrouter-observe`, the crate
PR 1 deleted. Its migration is overdue independently of every phase here, and
withdrawing phases 3–4 does not reduce it.

Splits the one `otel` module into three tiers so the OTLP renderer becomes the
swappable part rather than the load-bearing one, and so a consumer that wants
BitRouter's span semantics without OTLP has a path that is not "read
`exporter.rs` and re-derive it".

> **Read this before the rest of the document.** The sections below are kept in
> their original, pre-decision voice, because the reasoning is what a future
> reader needs in order to reopen the question well. But **the physical split
> did not happen and is not going to.** The goal in the paragraph above was met
> a different way: the *schema* became a committed, conformance-checked
> artifact (D2, shipped), which is what actually gives a second renderer
> something to implement. Moving files between crates was the means, not the
> end, and it was withdrawn on measured benefit — see *The cloud question*.
> Everything about tier 3's placement, the dependency move, and the `+N crates`
> arithmetic is **historical**. Do not act on it.

**State the reversal plainly.** Tier 3 lands in `apps/bitrouter` — the home
issue #808 proposed and the previous spec rejected by name. That rejection was
not about access but weight: `OTEL_SDK_MIGRATION_SPEC.md:76-83` records that
`apps/bitrouter`'s tree is *"408 crates against the SDK's 154 with
`otel-http`, so a consumer taking the exporter from the binary would take
sea-orm, ratatui, clap and the whole CLI with it, on the CLI's release
cadence."* Both counts still reproduce. This spec **is** a partial adoption of
issue #808, and it owes that objection an answer — see *The cloud question*
below. Do not read the tier table without reading that section.

*(Postscript: the answer that section eventually gave was "don't move it at
all", which moots the placement debate this paragraph opens. Issue #808 is
therefore **not** adopted, partially or otherwise, on the placement question —
only on the schema question, which it did not raise.)*

## Why

Three measurements motivate this. All are reproducible; see *Evidence* below.

1. **The instrumentation tier is nearly free; the exporter tier is everything.**
   Adding `otel-http` to `bitrouter-sdk` costs **+30 crates** (124 → 154).
   Expressing the same instrumentation against the `opentelemetry` **API crate
   alone** costs **+1 crate**, because `tracing`, `tracing-core`, `futures-core`,
   `futures-sink`, `pin-project-lite` and `thiserror` are already in the default
   tree.

   **Name the beneficiary, or don't claim one.** `default = []`, so a consumer
   who does not want OTLP already pays **+0** today — the 30 crates are opt-in
   and always have been. No workspace member other than `apps/bitrouter`
   enables any `otel` feature, and the one known out-of-tree consumer
   (`bitrouter-cloud`) *wants* OTLP. The +30 figure is therefore a bound on a
   **hypothetical** consumer, not a cost anyone is paying. This measurement
   motivates the shape; it does not on its own justify the work.

   **This paragraph turned out to decide the document.** The hedge was right
   and it was load-bearing: no beneficiary ever materialised, and when the
   remaining justification (build-cache invalidation) was finally measured it
   came out at roughly zero. D1 is withdrawn on exactly this basis. Both
   figures are also stale — 124/154 no longer reproduces; it is 159/201 today.
   See *The cloud question* and *Cost*.

2. **The doctrine is broader than the code supports.** Of ~2,556 production
   lines under `src/otel/`, a whole-file attribution gives roughly **66% span
   semantics, 22% transport and vendor glue, 12% deployment configuration**;
   attributing *within* `exporter.rs` shifts it toward 43/40/17. Both splits
   are directional only — no line is uniquely assignable, and line volume is
   evidence about weight, not about coupling. It does not by itself argue for a
   module split. The concrete point is smaller and sharper: `exporter.rs:827-836`
   stamps `$screen_name` because *PostHog's* "URL / Screen" column reads it — a
   named vendor's key inside the module D1 calls an open standard.

   `OTEL_SDK_MIGRATION_SPEC.md:45-60` already concedes both the ratio and the
   `$screen_name` example. Neither is new evidence; they are restated here only
   because D1 acts on them where the predecessor did not.

3. **The renderer is not a settled bet.** `opentelemetry-rust`'s own README
   marks Metrics-API/SDK and Logs-API/SDK `Stable` but **Traces-API,
   Traces-SDK and Traces-OTLP-Exporter all `Beta`** — and traces are what
   BitRouter leans on hardest. Upstream, the `gen_ai.*` move out of the main
   semconv registry is **complete, not pending**: `model/gen-ai/` now holds only
   `deprecated/`, whose notes read "Moved to the OpenTelemetry GenAI semantic
   conventions repository." The destination repo is live, actively pushed, and
   carries no releases or tags — a separate cadence with everything still at
   `stability: development`. OpenInference is a live competing vocabulary. OTLP
   as a transport has won; the LLM vocabulary above it has not.

None of this makes SDK placement wrong. It makes *one renderer welded to the
schema* the wrong shape for a surface that will churn.

## Decisions

### D1 — tier the dependency the way `opentelemetry-rust` tiers it — WITHDRAWN

**Withdrawn — see *The cloud question*.** Tier 1 shipped anyway, as D2's
artifact; tiers 2 and 3 stay where they are. The section is kept whole because
the tier *vocabulary* is still how this codebase talks about the module, and
because the constraints below (tracer binding order, the file inventory) are
true statements about the code that a future reader would otherwise rediscover
the hard way. **The "Home" column is the withdrawn part.**

Upstream is explicit: `opentelemetry` is *"the API crate, and is the crate
required to instrument libraries and applications"*, distinct from
`opentelemetry-sdk` and `opentelemetry-otlp`. Adopt that split verbatim.

| Tier | Contents | Home | Dependency |
| --- | --- | --- | --- |
| **1 — schema** | span names, the `bitrouter.*` vocabulary, requiredness, invariants | `bitrouter-sdk` | none |
| **2 — emission** | span/metric creation, W3C propagation (extract/inject), the ingress SERVER span | `bitrouter-sdk`, `all(__otel-core, server)` for ingress | `opentelemetry` (API) |
| **3 — export** | provider, sampler, batch processor, propagator *installation*, endpoint, credentials, transport | ~~`apps/bitrouter`~~ → stays in `bitrouter-sdk` | `opentelemetry-sdk`, `opentelemetry-otlp` |

Two corrections to the naive reading of that table, both load-bearing:

- **The ingress span is not reachable under `otel` alone.** It is gated
  `all(__otel-core, server)` (`otel/mod.rs:33-35`), and D3's replacement is
  still a tower layer needing `dep:tower`. A `--features otel` build contains
  no ingress span at all, so tier 2's contents and any "+1 crate" measurement
  describe different feature sets. Say which one a given gate means.
- **Propagator installation is an SDK-crate concern, not an API-crate one.**
  `exporter.rs:176` installs `TraceContextPropagator` from
  `opentelemetry_sdk::propagation`. Extraction and injection are API-crate
  operations; *installing* the global propagator is not. That call must move to
  tier 3 for any "no `opentelemetry_sdk` in tier 2" gate to hold.

#### Every file needs a home

The three-row table is not a plan until each existing file has a destination.

| File | Tier | Note |
| --- | --- | --- |
| `exporter.rs` | **splits** | span construction → 2; provider/processor/sampler build in `new()` (`:167-197`) → 3 |
| `acp.rs` | 2 | recorder; see the tracer-binding constraint below |
| `metrics.rs` | 2 | instrument creation; **provider installation** → 3 |
| `span_attributes.rs` | 1 | but see D2 — it is an unvalidated open region |
| `cardinality.rs` | 2 | pure limiter, no OTel SDK type |
| `http_layer.rs` | 2 | rewritten by D3 |
| `config.rs`, `transport.rs`, `auth_client.rs`, `bearer.rs`, `processor_runtime.rs` | 3 | deployment + transport; `bearer.rs` is the "credentials" cell |
| `subscriber.rs` | **blocked** | D4 |

#### The tracer must not be bound before the provider is installed

The Rust API crate does **not** behave like `@opentelemetry/api`. JS late-binds
through `ProxyTracerProvider`, whose `ProxyTracer` re-queries the delegate on
every `startSpan`, so a tracer handed out before SDK init starts working after
it. Rust resolves eagerly:

```rust
// opentelemetry-0.32.0/src/global/trace.rs:355-357
fn tracer_with_scope(&self, scope: InstrumentationScope) -> Self::Tracer {
    BoxedTracer(self.provider.boxed_tracer(scope))
}
```

`tracer_provider()` clones an `Arc` snapshot out of the `RwLock` at call time
(`:374-382`); there is no proxy and no re-resolution. **A `BoxedTracer`
obtained before `global::set_tracer_provider` wraps a `NoopTracer` for its
entire lifetime**, and drops every span silently.

That makes initialisation order a correctness constraint, not a detail:

- Tier 3 **must** call `set_tracer_provider` before any tier-2 type that
  stores a tracer is constructed, **or** tier 2 must acquire the tracer at
  span-creation time rather than storing it.
- `global::set_tracer_provider` has **zero** call sites in the workspace
  today. This is net-new wiring with no existing ordering discipline to
  inherit, and the failure mode is silent.
- The `AcpSpanRecorder`-stores-`BoxedTracer` shape in *Evidence* compiles.
  Compiling is not the risk; construction order is.

**The rule, restated:** the SDK owns the schema and emits against the API
crate; it does not own the transport. This *re-applies* the previous spec's D1
boundary rather than replacing it — auth, policy, charging, metering and
content policy stay out of the SDK, exactly as before.

#### The cloud question

`bitrouter-cloud` links `bitrouter-sdk` and enables an `otel` transport
feature. `OTEL_SDK_MIGRATION_SPEC.md:102-108` names that single consumer fact
as *"the load-bearing argument for where this lives."* Moving tier 3 into
`apps/bitrouter` does not delete that requirement; it relocates it into a
408-crate binary crate that cloud cannot reasonably depend on.

The options were:

1. Tier 3 becomes its own thin published crate (`bitrouter-otel`) that
   `apps/bitrouter` and cloud both take — the "keep it a separate published
   crate" option `OTEL_SDK_MIGRATION_SPEC.md:76-78` says was the question
   actually live, and which neither spec has yet answered on the merits.
2. Cloud reimplements tier 3 against the D2 artifact and the D5 conformance
   suite — which makes D5 a **prerequisite**, not a deferred nicety.
3. Tier 3 stays in `bitrouter-sdk` behind the existing default-off feature and
   only tiers 1–2 are extracted, leaving today's dependency weight unchanged.

##### ANSWERED: option 3. Phase 3 is withdrawn.

The question is closed, and not because cloud refused to migrate — the owner
was willing, which is what makes the answer worth recording. **Availability was
never the binding constraint. Benefit was.** Both of phase 3's justifications
were put to measurement and neither survived.

**1 — the crate-count argument has no beneficiary.** `default = []`, so this
document's own *"Name the beneficiary, or don't claim one"* paragraph already
conceded the +30 (now +42) figure is a bound on a hypothetical consumer. Under
option 1, run the ledger for the consumers that exist:

| Consumer | today | after phase 3 |
| --- | --- | --- |
| `apps/bitrouter` | wants OTLP → 201 crates | `bitrouter-sdk` + `bitrouter-otel` → the same |
| `bitrouter-cloud` | wants OTLP → 201 crates | `bitrouter-sdk` + `bitrouter-otel` → the same |
| a consumer wanting the schema without OTLP | **already +0** | +0 |

Nobody gets lighter. Phase 3 moves crates between nodes; it deletes them for
no one. Cloud's willingness to migrate cannot change this, because cloud
*wants* OTLP.

**2 — the graph-position argument is measured at roughly zero.** This was the
better argument, inherited from `OTEL_SDK_MIGRATION_SPEC.md:95-97` (*"any OTel
bump invalidates the build cache for the whole downstream workspace"*), and it
had never been costed. Measured with `cargo build --workspace --all-features`,
dev profile, warm cache, invalidating via `cargo clean -p` (no-op baseline
0.6s):

| Invalidated | Time |
| --- | --- |
| whole OTel family (api, sdk, otlp, proto, http, semconv, bridge) | **22.8s / 23.2s** |
| only what *stays* in `bitrouter-sdk` after phase 3 (api + bridge) | **10.9s** |
| only `opentelemetry_sdk` + otlp + proto + http | 23.8s |
| the six workspace crates | 18.2s |
| `apps/bitrouter` alone | **27.3s** |

Three readings, in descending order of how much they matter:

- **A real OTel release saves nothing.** The family ships in lockstep — every
  crate in our lockfile is 0.32 — so a bump invalidates the API crate, which
  after phase 3 *still* roots at `bitrouter-sdk` with all six in-repo
  dependents, because tier 2 emits against it by design. Row 2 measures that
  residue: about **half the cost is incurred either way**. The other half does
  not vanish either — the workspace still holds a crate that needs OTLP, so
  `opentelemetry_sdk` and `opentelemetry-otlp` still compile in the same
  `cargo build`. They change node; they do not leave the build.
- **The helped case is narrow.** An SDK-only patch — real, and visible in our
  own lockfile as `opentelemetry_sdk 0.32.1` against `opentelemetry 0.32.0` —
  would stop rebuilding `bitrouter-sdk` and its five downstream crates. That
  saving is bounded by row 4's 18.2s and is materially less, since
  `apps/bitrouter` relinks in both worlds.
- **Scale ends it.** Row 5 is the decisive number: rebuilding `apps/bitrouter`
  alone costs **more than the entire OTel-family scenario**. The app crate
  dominates every rebuild and rebuilds in every scenario. Phase 3 argues over
  ~10 seconds inside a build with a ~27-second floor, on an event that happens
  a few times a year. On CI it is worse — a cold cache compiles everything
  regardless of graph shape, so the saving there is exactly nil.

Discount for one machine, dev profile, and incremental-cache effects that make
rows 4 and 5 non-additive. The gap is not close enough for that to matter.

**What phase 3 was really for was delivered by phase 0.** The opening
paragraph's goal is a swappable renderer, because traces are Beta and
`gen_ai.*` now moves on its own cadence. A second renderer needs a
*specification*, and one now exists — declared, committed, and conformance-
checked. Relocating files buys dependency-graph shape, which is measured above
at approximately nothing. It does not buy swappability, because swappability
already shipped.

Option 2 is rejected on separate grounds and would be even if the numbers had
come out the other way: a cloud-side reimplementation creates a second renderer
of a schema whose entire value is being identical everywhere, and makes D5 a
prerequisite for the privilege.

**Consequence: this document ends at phase 2.** Phases 3 and 4 are withdrawn,
not deferred — see *Migration*. Reopen only on the trigger in *Abandon
triggers*, or if `bitrouter-otel` is wanted as a **published product surface**
that consumers adopt without the SDK. That is a positioning decision needing
different evidence than this section gathered, and it should not borrow this
section's arguments.

### D2 — the span schema becomes a committed artifact

Today the schema exists only as call sites: **91 `KeyValue::new` calls** in
production code (`exporter.rs` 71, `acp.rs` 11, `metrics.rs` 9) plus 46
`set_attribute` sites, across 55 distinct keys. Extract it to a versioned,
serializable declaration — span names, attribute keys, types, requiredness, and
the invariants that fail silently — and diff it in CI.

Note the shape difference from the obvious precedent: `public-api-deps.txt`
deliberately tracks the crate *set*, not the item listing, and says so in its
own header. A schema artifact needs the opposite design — the full item
listing, where every line is significant. Do not copy that file's diff
discipline without inverting its granularity.

**What this decision does and does not buy.** It does not unlock D1. An earlier
draft claimed `OTEL_SDK_MIGRATION_SPEC.md` concluded "the exporter must ship in
the SDK" from the schema-identity premise, and that extracting the schema
dissolves that inference. The predecessor draws no such inference. It says the
opposite at `:38-42` — *"**The OTLP renderer ships alongside it because there
is exactly one renderer and it is default-off** — not because OTLP transport,
bearer refresh, batch processing or endpoint configuration are SDK concerns in
their own right"* — and names its real reason at `:102-108`: the
`bitrouter-cloud` consumer fact, *"and not the 'domain model rendered into an
open standard' sentence."*

A schema artifact answers neither of those. **D2 does not make D1, D3 or D4
optional; the cloud question does.** D2 is worth doing on its own merits — a
reviewable, diffable schema is the cheapest guard in this document — but it
must not be sold as the keystone.

#### The artifact has a hole in it before it is written

`span_attributes.rs` ships `pub struct SpanAttributes(pub Map<String, Value>)`,
documented for *"A deployment (e.g. `bitrouter-cloud`) that computes attributes
the SDK does not know about"*, whose *"keys are used verbatim as span-attribute
keys."* The exporter stamps them unvalidated, and the SDK's own test injects
`bitrouter.retry_count` — a key **inside the `bitrouter.*` vocabulary tier 1
claims to own**.

So phase 0's gate ("artifact matches emitted spans") must either ignore this
class of key — gutting the guarantee for exactly the cross-deployment case D2
targets — or fail on any deployment that uses the feature. D2 must therefore
also decide one of:

- declare an **open extension region** (e.g. reserve a prefix, and forbid
  `bitrouter.*` from `SpanAttributes`), and state what "identical across
  deployments" still means with it; or
- narrow the hatch to a validated key set.

Do this first. It is independent of D1, D3 and D4, and correct even if none of
them ship.

#### Phase 0, as built

**Decision: the open extension region.** A validated key set would have to grow
for every deployment-specific attribute, which is the coupling `SpanAttributes`
exists to avoid.

- `crates/bitrouter-sdk/src/otel/schema.rs` declares every span, attribute,
  event, metric and invariant, and names no `opentelemetry` type — it is the
  tier-1 artifact, sitting inside the `otel` gate only because that is where a
  reader looks for it. It carries no dependency, so it could be lifted out of
  the gate untouched should a consumer ever want the schema without the
  feature — a property, not a plan; phase 3 is withdrawn.
- `crates/bitrouter-sdk/span-schema.json` is rendered from it and committed;
  the ordinary test run fails when it is stale, so **no new CI job was
  needed**. Regenerate with `UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-sdk
  --all-features committed_artifact`.

  The obvious home — `dist/schema/`, beside the config JSON Schema, rendered
  by `dist-helper` — **does not work, and the reason is a guard this spec did
  not account for.** `dist-helper` would have to enable `bitrouter-sdk/otel`
  to read the declaration, and `feature-isolation` (`ci.yml:305`) names
  `dist-helper` in the loop asserting that no workspace consumer reaches
  `opentelemetry` when resolved on its own. Verified, not assumed: with the
  feature added, `cargo tree -p dist-helper -e normal -i opentelemetry`
  succeeds and the job fails. The artifact therefore sits beside
  `public-api-deps.txt` — the crate's other generated, review-facing manifest
  — with one deliberate difference: it is **not** in the manifest's `exclude`
  list, because unlike the CI-only files it is the interop surface and ships
  with the crate.
- Reserved region: the `bitrouter.` / `gen_ai.` prefixes plus every key the
  declaration names. Reserved keys arriving through `SpanAttributes` are
  **dropped, not stamped**, with one DEBUG line each on the pinned
  `bitrouter::observe::span_attributes` target (`docs/CLI.md`, in lockstep).
  "Identical across deployments" therefore means: the declared vocabulary means
  the same thing everywhere, and everything outside it is the deployment's.
- Two conformance tests drive real lifecycles — a streamed pipeline with
  content capture on, and a failure path — and assert that every exported span
  resolves to a declaration, carries no undeclared attribute or event, and
  carries every required one. `acp.rs` has its own. Both were checked against
  injected violations in each direction (undeclared key; missing required key),
  so the guard is not vacuous.

What phase 0 did **not** do, per D2's own warning: it does not unlock D1, D3 or
D4, and the +30-crate reduction is untouched. The cloud question still governs.

One pre-existing test was changed, and not cosmetically.
`streamed_pipeline_exports_canonical_latency_and_first_token_timing` asserted
the hop's `bitrouter.upstream_duration_ms` *equal* to the root's. They come from
two different clocks — `ObservedUpstreamStream::provider_started_at` versus the
executor's own `started.elapsed()` in the finalized `ExecutionResult` — so the
equality held only while both rounded to the same millisecond. Adding two tests
to the binary was enough parallel load to make it fail about one run in three.
It now asserts the two agree within a few milliseconds. The test was latently
flaky before this change; it is not flaky because of it.

### D3 — the HTTP ingress span is an OTel span, not a `tracing` span

`http_layer.rs:32` builds a `tracing` span via tower-http's `TraceLayer` and
calls `OpenTelemetrySpanExt::set_parent` on it at `:93`; `exporter.rs:428` then
reads `tracing::Span::current().context()` to parent `chat` beneath it.

There are **three** functional uses of `tracing-opentelemetry`, not two. The
third is `subscriber.rs:51` — `tracing_opentelemetry::layer().with_tracer(…)`,
the bridge constructor that makes the other two work, and D4's subject. It is
also the only one compiled in *every* OTel build: `otel/mod.rs:39` declares
`pub mod subscriber;` unconditionally, while `http_layer` at `:33-35` is
`server`-gated. **D3 removes two of the three; the dependency survives on the
third.** That ordering consequence is spelled out in *Migration*.

Replace with a tower layer that extracts W3C context, starts a `SpanKind::Server`
span through the API crate, and publishes it on the OTel `Context` for the
request future via `FutureExt::with_context`. Downstream parents off
`Context::current()`.

**Prototype-verified at runtime** (see *Evidence*): parentage survives two
suspension points, the inbound `traceparent` is honoured, and the tree carries
no `tracing-opentelemetry`, no `tracing-subscriber` and no `tower-http`.

#### Phase 2, as built

Shipped as described, as an `axum` middleware rather than a hand-rolled tower
`Service`. Four things the plan did not have right:

- **The layer needs the exporter.** `router_wrapper()` became
  `router_wrapper(&OtelExporter)`. "Start a `SpanKind::Server` span through the
  API crate" has no tracer to start it *from*: the SDK installs no global
  `TracerProvider`, so `global::tracer()` returns a `NoopTracer` and the span
  is dropped silently. This is the same constraint D4 hit, arriving from the
  other direction, and it is why the prototype's "no `opentelemetry_sdk` in
  this path" reading never held.
- **The bridge read in `exporter.rs` stays.** D3 claimed to remove two of the
  three `tracing-opentelemetry` uses. It removes one. `exporter.rs`'s
  `tracing::Span::current().context()` arm is how a host that builds its own
  `tracing` ingress span reaches the exporter, and phase 1 established that
  `bitrouter-cloud` is exactly that host — removing it would have exported
  orphaned `chat` roots for cloud, silently. It is now an explicit
  precedence chain (native → bridge → inbound `traceparent`) with a test per
  arm. The dependency argument for removing it evaporated anyway when D4 kept
  the bridge and D1 was withdrawn.
- **`tower-http` is gone, and with it the weak-feature mechanic.**
  `server` no longer carries `tower-http?/trace`; `__otel-core` no longer
  carries `dep:tower-http`; the dependency is out of the manifest.
  `OTEL_SDK_MIGRATION_SPEC.md`'s section on it is marked retired.
- **With OTel disabled there is now no ingress span at all**, where the old
  `TraceLayer` ran unconditionally and built `tracing` spans that went nowhere.

**What this bought, stated precisely.** Not a dependency saving — D4 and the
D1 withdrawal had already spent that. It removes a silent failure mode:
`RUST_LOG=warn` used to orphan every `chat` span with no diagnostic anywhere,
and now cannot affect span export at all. `observe_hierarchy.rs` asserts the
full tree *under a blanket `warn` filter*, which is the inverse of the
filter it used to run.

**The gate the spec demanded, and why it is its own test binary.** The new
capturing-layer test lives at `crates/bitrouter-sdk/tests/ingress_log_target.rs`
rather than beside the other unit tests. `tracing`'s per-callsite `Interest`
cache is process-global: a sibling test exercising the same `debug!` callsite
with no subscriber installed causes `Interest::never` to be cached, and the
assertion then sees nothing. The in-module version passed alone and failed
about two runs in five in the suite. A separate binary is a separate process.
Verified non-vacuous by deleting the `target:` pin — it fails, reporting the
`bitrouter_sdk::otel::http_layer` module-path fallback that the pin exists to
prevent.

One more thing phase 2 changed that the spec did not foresee:
`observe_hierarchy.rs`'s poll helper waited on the SERVER span because under
`tower-http` it closed *last*. The OTel-native span closes as soon as
`next.run` returns — for a streamed response, when the body starts rather than
when it drains — so the ordering inverted and the helper began shutting the
pipeline down before the root `chat` span existed. It now waits on both spans
it asserts on, which is invariant to the ordering.

Consequence worth stating: the weak-feature mechanic recorded in the previous
spec's D3 (`server = [..., "tower-http?/trace"]`, and its feature-intersection
argument) exists to gate `tower-http/trace` for `TraceLayer`. `http_layer.rs:30`
is the crate's only use of `tower_http`, so dropping `TraceLayer` retires both
the dependency and the mechanic. The replacement is still a tower layer and
still needs `dep:tower`, which `server` already carries — the ingress path does
not become feature-free, only `tower-http`-free.

### D4 — `tracing_subscriber_layer`: RESOLVED — it stays

`otel::subscriber::tracing_subscriber_layer` is the single reason
`tracing_subscriber` 0.3 and `tracing_core` 0.1 — two **0.x** crates, where
every minor is breaking — are public dependencies of the foundation crate.

D3 removes the bridge from *BitRouter's own request path*. It does **not**
establish that the SDK can stop *offering* the bridge. A consumer that wants its
**own** `tracing` spans in the same trace still needs this function, and
`bitrouter-cloud` is known to link `bitrouter-sdk` and enable an `otel`
transport feature.

**In-tree callers exist and must be migrated regardless of what cloud says:**
`apps/bitrouter/src/main.rs:2667` (`init_serve_tracing_subscriber`) and
`apps/bitrouter/tests/observe_hierarchy.rs:65`. The cloud grep decides
delete-vs-keep, not whether work is needed.

**A third consumer is easy to miss:** `.github/workflows/ci.yml:406` pins the
literal string `'pub fn bitrouter_sdk::otel::subscriber::tracing_subscriber_layer'`
as one of two sentinels proving the `sdk-public-api` listing is non-vacuous.
Deleting the function fails that step *before* the deps diff is computed — so
removal deletes the vacuity guard rather than repairing it. **D4's phase must
swap in another proven-public `otel` item in the same commit.**

**The blocking question is answered: hits.** Run against `bitrouter-cloud`
`origin/main` at `e224842f` (v0.23.0):

```
src/main.rs:7:use bitrouter_observe::otel::http_layer::tracing_subscriber_layer;
src/main.rs:1624:        Some(exp) => registry.with(tracing_subscriber_layer(exp)).init(),
```

So: **it stays, and its cost stays with it.** `tracing_subscriber` 0.3 and
`tracing_core` 0.1 remain public dependencies of the foundation crate. That is
now a settled cost, not an open question — stop pricing phase 3 as if removing
it were available.

Three things the grep turned up that change more than the branch:

1. **Cloud is not on the migrated SDK at all.** It imports from
   `bitrouter_observe` — the crate PR 1 deleted — and from `http_layer`, the
   module the same PR split the bridge *out of*. Its manifest pins
   `bitrouter-observe = { version = "1.0.0-alpha.27", features = ["otel-http"] }`
   alongside `bitrouter-sdk = "1.0.0-alpha.27"`. It builds only because
   crates.io is append-only. **Cloud has an outstanding migration from PR 1
   that predates this document**, and every "sequence it into one release
   window with cloud's migration" sentence below inherits it.
2. **Cloud owns its ingress span on purpose, and does not use
   `http_layer::router_wrapper`.** `src/main.rs::server_span_layer` installs
   its own `TraceLayer` and deliberately does *not* bind inbound W3C context —
   the comment's reason is that a public multi-tenant edge must not let callers
   control its trace ids, sampling, or what leaks to upstream providers. The
   bridge is what maps that `tracing` span into OTel, so D3 removing
   BitRouter's *own* two bridge uses does nothing for cloud. This is the
   strongest argument for the keep branch, and it is not the argument the
   section was written around.
3. **The one other cloud consumer of this module is `SpanAttributes`**
   (`src/v1/settlement.rs:19`), which is phase 0's subject, not D4's. Its keys
   are checked below.

#### The re-signature option: bound verified, option still dead

The live option this section listed — re-signature onto `BoxedTracer` — was
checked rather than assumed, and it splits in two:

- **The stale-rationale half is confirmed.** `PreSampledTracer` does not exist
  anywhere in `tracing-opentelemetry` 0.33. `with_tracer` asks only for
  `Tracer: opentelemetry::trace::Tracer + 'static` with
  `Tracer::Span: Send + Sync`, and `opentelemetry::global::BoxedTracer`
  satisfies it (`type Span = BoxedSpan`). `subscriber.rs`'s comment has been
  corrected; the signature is not forced by a trait bound.
- **The conclusion drawn from it does not hold.** "Serves cloud unchanged" is
  wrong twice. Cloud passes `&OtelExporter`, so its call site changes either
  way — minor. The fatal half is that **nothing installs the global tracer
  provider**: `global::set_tracer_provider` still has zero call sites, and
  `OtelExporter` deliberately builds a per-exporter provider (same reasoning
  `OtelMetrics` records for meters — installing globally clobbers any other
  OTel consumer in the process). A `BoxedTracer` taken today wraps a
  `NoopTracer` for its whole lifetime. The re-signature would compile, pass
  every existing test that does not assert on exported spans, and silently
  drop every bridged span.

So the re-signature is not a signature change with a semver trade-off. It is a
**decision to install the global provider**, carrying exactly the
silent-initialisation-order failure D1 already flags as its own largest
unverified risk. It is not taken here, and phase 3 must not plan around it
without first deciding the global-provider question on its own merits.

The other two options an earlier draft listed remain dead ends, and are
recorded so they are not re-proposed:

- ~~"Move it to tier 3"~~ puts it in the 408-crate binary the previous spec
  rejected as a library home — the same weight objection as D1, for the same
  consumer.
- ~~"Keep it `server`-gated"~~ keeps `OtelExporter` — and therefore
  `opentelemetry_sdk` — in the SDK, which blocks phase 3 outright.

**What phase 1 changed in the tree: the comment, not the code.**
`subscriber.rs`'s rationale asserted a bound that no longer exists, which would
have led the next reader to believe the signature was forced. It now records
what actually forces it — eager tracer binding with no global provider
installed — and points here. The signature, the return type, and
`ci.yml:406`'s sentinel are all unchanged: the sentinel swap was a
*removal*-branch requirement, and the removal branch did not happen.

**Acceptance, keep branch (the one taken).** `subscriber.rs` stays, so the
SDK's `otel` tier cannot reach a crate target computed without it. That
restated the phase-3 gate to exclude `tracing-opentelemetry`,
`tracing-subscriber` and `tracing-core`, which the bridge keeps in the tree by
design — and shrinking that target is part of what made phase 3 fail its own
cost/benefit test one section later. Any "+1 crate" or "154 → 124" figure
quoted for a hypothetical tiered SDK is wrong for as long as this function
exists; see *Cost*.

### D5 — an `ObserveHook` conformance suite — WITHDRAWN as a phase

`ObserveHook` (`language_model/hooks.rs:185`) is already a genuine extension
point — public trait, `Vec<Arc<dyn ObserveHook>>` so observers compose, and
`set_outbound_trace_headers` deliberately shaped so W3C propagation crosses the
seam without OTel types (`context.rs:394-400` hard-filters to `traceparent` /
`tracestate`). What it lacks is any way for an implementor to check they got it
right; `observe_hierarchy.rs` is an internal integration test.

Not all methods default, and the exceptions are the interesting ones: **three
of eight have no default body** and must be implemented — `after_phase`
(`:193`), `on_stream_part` (`:240`), `on_request_end` (`:244`). Those three
carry the span lifecycle, so they are precisely what a conformance suite has to
cover.

Ship a conformance suite behind a `testing` feature, asserting the observable
contract a hook must satisfy. Paired with D2, this answers "behaviour must be
identical across deployments" with *a test any implementation can run* rather
than *we ship the only implementation*.

**Acceptance:** a deliberately-wrong hook — one that omits a required
attribute, mis-parents `chat`, or drops `traceparent` — fails the suite; the
in-tree `OtelObserveHook` passes it unmodified.

~~**Not simply deferrable.** If D1 resolves the cloud question via option 2
(cloud reimplements tier 3), D5 is a **prerequisite** for phase 3, not a
post-script — it is the only thing that would make a second implementation
checkable. Under options 1 and 3 it stays a deferred nicety.~~

**D1 took option 3, so this reverts to "deferred nicety" — and phase 4 is
withdrawn.** The escalation was conditional on a cloud-side reimplementation
that is not happening. Two notes for whoever picks this up later:

- The gap D5 named is real and partly closed. Phase 0's committed artifact
  gives an implementor the required attributes and the invariants to satisfy;
  what it does not give them is a *runnable* check, which is D5's whole point.
- The acceptance bar above is still the right one. Build it when a second
  `ObserveHook` implementation actually exists, not before — the in-tree hook
  is already covered by the conformance tests phase 0 added.

## Evidence

Distinguish verified from asserted. Everything in the first table was executed
in a scratch prototype — see the reproducibility caveat below before citing it.

| Claim | How verified |
| --- | --- |
| API crate expresses the whole span tree — `span_builder`, `with_kind(Internal/Client/Server)`, `start_with_context`, `set_attribute`, `set_status`, `Context::current`, `with_span`, custom `Extractor`, `get_text_map_propagator().extract()` | compiled against `opentelemetry` alone |
| API crate expresses the metrics tier — `meter_provider().meter()`, `u64_counter`, `f64_histogram`, `u64_histogram`, `add`, `record` | compiled |
| `AcpSpanRecorder` can store its tracer as `opentelemetry::global::BoxedTracer` — an API type — with no `opentelemetry_sdk` type and no `pub(crate) tracer_clone()` | compiled — **compilation only; see the binding risk below** |
| A standalone tier-2 probe resolves to 13 crates total, all but one already in `bitrouter-sdk`'s 124-crate default tree (net **+1**); zero `opentelemetry_sdk` / `tracing-opentelemetry` / `tracing-subscriber` | `cargo tree -e normal` |
| Ingress handoff: SERVER parents off inbound `traceparent`; `chat` parents off SERVER; same trace id; across two `yield_now()` points; no bridge, no `tower-http` | runtime test, `CapturingProcessor` |
| 124 (default) / 154 (`otel-http`) crate counts | `cargo tree -e normal -p bitrouter-sdk --no-default-features [--features otel-http] --prefix none \| sort -u \| wc -l` |
| 4,425 lines under `otel/`, 1,869 `#[cfg(test)]`, 2,556 production | line count |

The 13-and-+1 row states two different measurements; do not read it as implying
the SDK's default tree is 12.

An earlier draft cited "`opentelemetry-otlp`'s own closure is 191 crates" and a
bare "166 / 408". **191 does not reproduce under any method** — in-workspace
`cargo tree -e normal -p opentelemetry-otlp` gives 131; a standalone probe gives
92 (default features), 148 (all features), 197 (`--target all`, all features).
The figure is withdrawn. 166 and 408 were attached to no claim in the body
(they reproduce as `otel-grpc` and `-p bitrouter`; 408 belongs to the previous
spec's weight argument, quoted at the top of this document).

**Not verified — these are the risks:**

- **Tracer binding order.** The `BoxedTracer` row proves the type compiles as a
  struct field. It does **not** prove a tracer bound before
  `set_tracer_provider` ever emits — it does not (see D1). No prototype covers
  construction order, and the failure is silent span loss.

- **End-to-end equivalence.** The prototype proves the mechanism, not that the
  emitted tree is identical. This is the acceptance gate and there is no
  cheaper way to get it. **No single test covers the tree this spec names** —
  see *The acceptance gate is three tests, not one* below.
- **Reproducibility.** The prototype is scratch-only: no branch, no commit, no
  recorded invocations. A reviewer cannot re-run any row in the table above.
  Either land it as a referenced branch or treat those rows as author
  assertions rather than evidence.
- **`observe_hierarchy.rs` cannot prove what it is being asked to prove after
  D3** — its guard is structural and D3 severs the structure. See *Constraints*.
- **Response-side attributes.** The prototype records request attributes only;
  `http.response.status_code` and the error path are unimplemented.
- **gRPC.** Only the HTTP path was prototyped.
- **Metrics provider installation** at tier 3 (the meter *API* was probed, not
  the provider wiring or the periodic reader).

### The `tracer_clone` constraint is resolved, not worked around

The previous spec records: *"The original issue proposed moving `acp.rs` into
`apps/bitrouter`. That is impossible… the recorder stores that tracer as a
struct field. Across a crate boundary this forces `tracer_clone` public."*

`opentelemetry::global::BoxedTracer` is the type-erasing handle that claim
assumed did not exist. A recorder storing it as a field compiles, names no SDK
type, and needs no `tracer_clone`. **Retire that argument; do not reuse it.**

But retiring it replaces a *type* constraint with an *ordering* one. Because
`BoxedTracer` binds eagerly (D1), a recorder that stores one is correct only if
it is constructed after `set_tracer_provider`. The `tracer_clone` problem is
solved; a silent-initialisation-order problem takes its place, and it is
strictly harder to catch — the type system enforced the old one.

## Constraints

### The frozen `RUST_LOG` targets do not survive D3 for free

**Only `bitrouter::observe::http` is at risk.** `bitrouter::observe::cardinality`
is emitted at `cardinality.rs:40-43`, a `tracing::warn!` in
`CardinalityLimiter::cap`'s poisoned-lock recovery arm — nowhere near the
ingress span, and untouched by D3. `observe_hierarchy.rs` contains zero
occurrences of the string "cardinality". Scope the remediation to one target,
not two.

For `bitrouter::observe::http`: the current SERVER span carries that target
*because it is a `tracing` span* (`http_layer.rs:80`, plus a `tracing::debug!`
at `:95` on the extract-failure path). An OTel-native ingress span carries
none, so D3 must emit them explicitly —
`tracing::debug!(target: "bitrouter::observe::http", …)`. One line per site.

**A green `observe_hierarchy.rs` is *not* the check, and this is the trap.**
That test has no `EnvFilter` assertions: the filter is constructed at line 61
and never asserted on. Its guard is *structural*, and documented as such at
lines 47-52 — drop the explicit target pin and the span falls back to a
`bitrouter_sdk::…` module path, which `bitrouter_sdk=warn` filters out, which
starves the bridge, which makes the SERVER-span lookup panic. **D3 severs
exactly that chain.** An OTel-native span never passes through the `tracing`
subscriber, so `EnvFilter` cannot suppress it, and the SERVER assertion goes
green whether or not the target pin still exists. The regression test does not
fail — it stops testing.

Two consequences:

- Phase 2 needs a **new** test: a capturing `tracing` layer asserting the
  target string and level directly, independent of span export.
- The replacement events are `debug!`, and the test's filter is
  `EnvFilter::new("info,…")` — which would not enable them anyway. The new
  test must set its own level.

`docs/CLI.md:40` and `:46` document this as an operator contract ("a filter
that drops `bitrouter::observe::http` at INFO also drops the SERVER span").
After D3 that sentence becomes false — the ingress span no longer flows through
`tracing` at all. **CLAUDE.md requires CLI-surface docs to move in lockstep, so
phase 2 is not "Breaking? no" for the documented contract.**

### Everything already frozen stays frozen

`plugins.bitrouter-observe`, the `BITROUTER_OBSERVE_*` env names, the `/metrics`
banner, `io.bitrouter.observe`, the `bitrouter` meter name, and the pinned log
target **strings** are unchanged by every decision here. `config.plugins` is an
unvalidated `HashMap<String, Value>`, so a renamed key still parses and
telemetry stops silently.

"Frozen" here means the strings, not their semantics: the section above records
that `bitrouter::observe::http` changes what it carries and what suppressing it
does. Both statements are true; do not read this one as overriding that one.

### The acceptance gate is three tests, not one

`observe_hierarchy.rs` does **not** assert the SERVER → `chat` →
(`route`, hop, `settle`) hierarchy. Those names appear in its prose only. Its
complete assertion set is: instrumentation scope non-empty, `scope.name ==
"io.bitrouter.observe"`, `scope.version == CARGO_PKG_VERSION`, some
`trace_id.len() == 16`, a `SpanKind::Server` span exists, a
`name == "chat test-model"` `Internal` span exists, `chat.parent_span_id ==
server.span_id`, and matching trace ids. Its collector is a
`wiremock::MockServer` — the previous spec's "a live OTLP collector stub" was
the more careful phrasing.

The rest of the tree is proved elsewhere, and the two proofs do not overlap:

| Shape | Where |
| --- | --- |
| SERVER → root `chat`, scope name/version | `observe_hierarchy.rs` |
| `route` / `settle` parent on root `chat` | `full_stack.rs:634-655` — builds with plain `build_router(state)` (`:212`), so it has **no SERVER span at all** |
| `route` / `settle` parenting, in-crate | `exporter.rs:2000` |

**No single test covers the tree this spec names, and nothing covers the
ingress seam and the sub-tree together.** Every migration gate below must name
all three, and phase 1 — which swaps the tracer that emits `route`/hop/`settle`
— must not be gated on the one test that never looks at them.

### `bitrouter-cloud` is a real out-of-tree consumer

Read at `origin/main` `e224842f` (v0.23.0), so this is inventory, not
inference. Its whole surface against this module is three imports:

| Cloud call site | Uses | Bears on |
| --- | --- | --- |
| `src/main.rs:7` | `otel::http_layer::tracing_subscriber_layer` | D4 — decided the keep branch |
| `src/main.rs:8` | `otel::{MetricsConfig, OtelConfig, OtelExporter, OtelObserveHook, SamplerKind}` | D1 — these are the re-exports needing a destination |
| `src/v1/settlement.rs:19` | `otel::SpanAttributes` | D2 — checked below |

Three consequences, in order of how much they change the plan:

- **Cloud still depends on `bitrouter-observe`, which no longer exists.** Its
  manifest pins that crate at `1.0.0-alpha.27` and imports the exporter, the
  config types and `SpanAttributes` from it. Cloud's migration debt is
  therefore *already* one PR deeper than this document assumed, and it is not
  optional: the next `bitrouter-sdk` bump it takes will not have that crate to
  pair with. **This is due regardless of whether phases 1–4 ever ship.**
- **Phase 0 is compatible with cloud, verified.** Cloud forwards exactly three
  keys through `SpanAttributes` (`src/v1/settlement.rs:552`):
  `$ai_total_cost_usd`, `byok` (a bool), `routing_profile`. None is under a
  reserved prefix and none collides with a declared key, so all three still
  land. The open-extension-region decision cost the one known consumer
  nothing; a validated key list — D2's other option — would have had to know
  these three names in advance and would have broken cloud the day it added a
  fourth.
- **Nothing here is breaking for cloud any more.** D4's keep branch leaves the
  bridge's signature intact, and D1's withdrawal leaves the exporter, the
  config types and `SpanAttributes` exactly where cloud's next SDK bump will
  find them — in `bitrouter_sdk::otel`. The only change cloud must make is the
  one it already owed: `bitrouter_observe::otel::…` → `bitrouter_sdk::otel::…`,
  plus dropping the dead `bitrouter-observe` dependency. Table row 1's import
  also moves module (`http_layer` → `subscriber`), which is the single
  non-mechanical edit in the set.

**D1's cloud question is answered — option 3 — so no crate changes hands and
no migration note is owed on that account.** The second table row was that
question in concrete form: five re-exported names, one consumer, no home yet.
They keep the home they have.

## Migration

| Phase | Change | Breaking? | Gate |
| --- | --- | --- | --- |
| **0** | D2 — schema artifact + CI diff + the `SpanAttributes` extension-region decision — **landed** | no | artifact matches emitted spans; all three tests green |
| **1** | D4 — resolve `tracing_subscriber_layer` (was phase 4) — **landed: it stays** | **no** (keep branch) | cloud grep answered: hits; sentinel unchanged, because nothing was removed |
| **2** | D3 — OTel-native ingress span; re-add the pinned `http` target; update `docs/CLI.md` — **landed** | **yes, for the documented operator contract** (and for `router_wrapper`'s signature, which the plan missed) | met: `tests/ingress_log_target.rs` asserts the target + level and fails without the pin; all three tests green |
| ~~**3**~~ | ~~D1 — the dep move~~ — **withdrawn**, see *The cloud question* | — | — |
| ~~**4**~~ | ~~D5 — conformance suite~~ — **withdrawn as a phase**, see below | — | — |

**Phase 2 is the entire remaining scope, and it lands on its own.** The
original sequencing note — "phase 0 lands independently, the rest do not,
because the tracer swap and the dep move are one breaking change" — described a
dependency on phase 3 that no longer exists. With D1 withdrawn there is no
tracer swap, no dep move, and nothing for phase 2 to be sequenced against.

**Why D5 is withdrawn as a *phase* rather than deferred.** It was promoted to
"prerequisite, not deferred nicety" only under D1's option 2, where a
cloud-side reimplementation would have needed something to check itself
against. Option 2 is not the answer taken. What remains of D5's value — giving
an implementor a way to verify a hook — is now substantially served by phase
0's artifact and the conformance tests behind it. Reopen it on its own merits
if a second `ObserveHook` implementation actually appears; do not carry it as
scheduled work for a phase that is not happening.

Two changes from the earlier ordering, each forced at the time:

- **D4 moves first.** `subscriber.rs:51` keeps `tracing-opentelemetry` — and
  the crates behind it — in the SDK's tree. Any "+1 crate" gate is unreachable
  while it stands, so resolving it *after* the dep move gates that move on its
  own unfinished prerequisite.

  **This ordering was right and its premise is now spent.** D4 resolved to
  *keep*, so `subscriber.rs:51` stands permanently and the "+1 crate" gate is
  permanently unreachable — not blocked, unreachable. Phase 3 does not inherit
  a prerequisite from phase 1; it inherits a smaller target. The restated gate
  is in D4's acceptance, and *Cost* is re-priced below.
- **Old phases 1 and 3 merge.** Phase 1 was marked "internal". It is not:
  `OtelExporter::new` (`exporter.rs:167-197`) is public and builds the
  propagator, processor, sampler and provider; moving that construction changes
  or removes the constructor, and `AcpSpanRecorder::new` and
  `tracing_subscriber_layer` both lose their parameter type. Landing it alone
  also leaves the SDK paying for the whole OTLP closure while constructing no
  provider — it can no longer export with the deps it still carries. There is
  no coherent tree between the two halves, so they ship together. *(Moot: both
  halves are withdrawn.)*
- **Phase 2 is breaking for the documented contract**, even though no signature
  changes. See *Constraints*. **This one still stands** and is the live
  consideration for the remaining work.

**`.github/workflows/ci.yml` needs no edit.** This section previously carried a
standing requirement that phase 3 rewrite the `feature-isolation` job in the
same commit, because that job's positive resolution guards assert the presence
of exactly the packages phase 3 would remove — `cargo tree -p bitrouter-sdk
--all-features -e normal -i <pkg>` exits 101 for a package absent from the
SDK's reachable set, so emptying `opentelemetry_sdk`, `opentelemetry-otlp` and
`tonic` out of that tree would fail with "misspelled in this list?", and
keeping them as dev-deps just moves the failure to the negative assertion.
With phase 3 withdrawn, none of that happens: the job, the `otel-http` /
`otel-grpc` `cargo check` steps and the `__otel-core` `compile_error` step all
stand unchanged, and the feature set they guard is now the permanent one rather
than a pre-change invariant.

Phase 0 was worth doing on its own and was done. Phase 2 has no external
dependency: it touches `apps/bitrouter`'s ingress path and `docs/CLI.md`, and
cloud does not use `http_layer::router_wrapper` — it builds its own SERVER span
— so phase 2 needs no release window with cloud and no migration note.

### Cost

Not previously stated, and it is not small. By this document's own file
attribution, roughly **1,450 production lines** leave `bitrouter-sdk` in phase 3
— `config.rs` (455), `auth_client.rs` (152), `processor_runtime.rs` (145),
`transport.rs` (128), `cardinality.rs` (103), `bearer.rs` (24), plus the
provider core of `exporter.rs` — against ~20 call sites in
`apps/bitrouter/src/assemble.rs` alone. Six public `otel::*` re-exports
(`otel/mod.rs:42-48`: `TelemetryBearer`, `CardinalityLimiter`, the `OtelConfig`
group, `OtelExporter`, `OtelObserveHook`, `OtelStatus`, `SpanAttributes`) need a
stated destination; the previous spec built the entire `sdk-public-api` job
around that surface.

Weighed honestly, the trade was: **a four-figure-line relocation plus a
breaking release for cloud, to remove some opt-in crates that no current
consumer pays for.** Phases 0 and 2 carry most of the durable value — a
diffable schema and an ingress span that does not depend on a `tracing` bridge
— at a fraction of that cost.

**That trade was declined.** The sentence this section ended on — *"if the
cloud question resolves to option 3, stopping after phase 2 is the better
outcome, not a failure"* — is the outcome. It resolved to option 3 on measured
benefit rather than on cloud's unavailability; the ledger and the timings are
in *The cloud question*. The cost side below stands as the record of what was
not spent.

**"30 crates" is withdrawn on both halves.** Re-measured on the current tree
with this document's own command:

| | crates |
| --- | --- |
| `--no-default-features` | **159** (was 124) |
| `--features otel-http` | **201** (was 154) |
| distinct package names added | **36** |

The 124/154 pair no longer reproduces — the default tree grew ~35 crates
underneath this document while it sat unimplemented, which is itself a reason
not to quote a dependency figure without re-running it.

The second half matters more: **D4's keep branch makes part of that 36
permanent.** Nine of the added names are reachable from the bridge
(`tracing-opentelemetry`, `tracing-subscriber`, `tracing-log`,
`sharded-slab`, `thread_local`, `lazy_static`, plus `opentelemetry`,
`tracing-core` and `thiserror`, which have other dependents too). The last
three would stay in tier 2 regardless — tier 2 emits against the API crate by
design — so the bridge's *marginal* permanent cost is the first six. Phase 3's
achievable reduction is therefore materially smaller than any figure this
document has quoted, and the exact floor is not stated here because the only
measurement of it came from the scratch probe *Evidence* already flags as
unreproducible. Re-measure before quoting a number; do not reuse 30, and do
not reuse 42.

The re-pricing is kept even though phase 3 is withdrawn, because the figure
outlived the plan once already: 124/154 was quoted here long after it stopped
reproducing. Anyone reopening this decision should start by re-running the
command, not by citing this table.

### Rollback

~~`crates.io`'s index is append-only, so a bad phase-3 release cannot be
withdrawn — only yanked and superseded. Before that release: land the phase-3
diff behind a branch that cloud can build against, and keep the previous SDK
minor supported until cloud's migration is merged.~~ **Moot with phase 3
withdrawn — and worth noting as part of what the withdrawal bought.** The one
irreversible step in this plan is gone; **phases 0 and 2 are revertible by
ordinary means**, and phase 0 is already in the tree behind an
artifact-plus-test guard that a revert removes cleanly.

## Acceptance

Criteria 1, 2 and 5 are the live ones; 4 and 6 retire with phase 3 and are
kept only so a reader does not mistake their absence for an oversight.

1. **The OTLP assertions still hold, with the wiring rewritten.** The earlier
   bar — `observe_hierarchy.rs` "unmodified except for import paths" — is not
   achievable and contradicts criterion 2: the test's setup installs
   `tracing_subscriber_layer` and `http_layer::router_wrapper()` (the
   `TraceLayer` path phase 2 replaces), and its function name and module doc
   both name the bridge as the mechanism under test. The honest bar is that the
   **assertions** survive verbatim — SERVER span present, root `chat` parented
   on it, matching trace ids, scope name and version — while the wiring, test
   name and doc comment are rewritten. Diff the assertion block, not the file.
   (Phase 1 kept `tracing_subscriber_layer`, so only the `router_wrapper` half
   of this now bites.)
2. All three tests in *The acceptance gate is three tests, not one* stay green
   at every phase, plus the new capturing-layer test from phase 2. ~~Phase 3 is
   additionally gated on `full_stack.rs` and `exporter.rs:2000`~~ — withdrawn
   with phase 3, though both remain the only proofs of `route` / `settle`
   parenting and phase 2 must not break them.
3. `public-api-deps.txt` diff is reviewed, not regenerated silently. ~~If phase
   1 removes `tracing_subscriber` and `tracing_core`, that diff is the
   deliverable~~ — **settled: phase 1 kept the function, so the file is
   unchanged and `ci.yml:406`'s sentinel stands.** `tracing_subscriber` and
   `tracing_core` are permanent public dependencies. Phase 0 confirmed the
   file is a live guard rather than a formality: it was regenerated and
   compared against all four gates on the pinned toolchain, and the new
   `otel::schema` surface reached no crate not already on it.
4. ~~`feature-isolation` passes **as edited by phase 3**.~~ **Retired.** With
   phase 3 withdrawn the job is not edited, and the criterion inverts: the
   feature set it guards — `otel-http`, `otel-grpc`, `__otel-core`, the
   transport-less `compile_error`, the per-transport `cargo check` steps — is
   now the **permanent** shape, not a pre-change invariant awaiting
   replacement. It gates phase 2 and everything after it, unmodified.
5. The schema artifact and the emitted spans agree, enforced in CI, with the
   `SpanAttributes` extension region explicitly in or out of scope. **Met by
   phase 0**: in scope, as an open region, enforced by a staleness test plus
   two conformance tests, each checked against injected violations.
6. ~~Every public `otel::*` re-export has a documented destination, and cloud
   has a written migration naming the crate its exporter now comes from.~~
   **Retired: nothing moves, so every re-export's destination is where it
   already is.** The cloud-facing obligation does not disappear, it changes
   subject — cloud's outstanding migration is off the deleted
   `bitrouter-observe` and onto `bitrouter-sdk`'s `otel` module, which this
   document does not own. See *`bitrouter-cloud` is a real out-of-tree
   consumer*.

## Abandon triggers

Stop and re-open the decision if:

- **Phase 2 cannot keep the assertion set green** across all three tests. The
  span tree is the product; a tiering that changes it is not worth having.
  Note the inverse failure too: a green `observe_hierarchy.rs` after D3 is
  *not* evidence the target pin survived — see *Constraints*. Do not read this
  trigger as satisfied by the old gate.
- **The cloud grep in D4 returns hits and cloud cannot migrate.** ~~Then the
  public-API liability stays regardless, and phase 3 loses most of its value —
  do phases 0–2 and stop.~~ **Half-fired, and read it carefully.** The grep
  returned hits, so the public-API liability does stay regardless — that half
  is now fact, and phase 3's value is correspondingly lower (see *Cost*). The
  second condition did not fire: cloud *can* migrate, and in fact already owes
  a migration from PR 1. So this is not yet "do phases 0–2 and stop" — it is
  "phase 3 is worth less than advertised, and D1's cloud question now carries
  the whole decision." If that question also fails, the trigger below is the
  operative one and the answer is the same: stop after phase 2.
- **D1's cloud question has no acceptable answer.** ~~If tier 3 can live
  neither in a thin published crate nor in a cloud-side reimplementation, phase
  3 is not executable at all.~~ **FIRED, and the outcome is the one this bullet
  prescribes: take option 3, keep the exporter where it is, and ship phases 0
  and 2 as the whole change.** Note it fired for a reason the bullet did not
  anticipate. Tier 3 *could* have lived in a thin published crate — that option
  was available, and cloud was willing to migrate to it. It fired on benefit,
  not feasibility: no consumer gets lighter, and the build-cache saving
  measures at roughly zero. A trigger written as "no acceptable home" was
  really "no demonstrable benefit"; whoever reopens this should test the second
  question, which is harder to answer and the only one that mattered.

Since the decision is now closed, the triggers that would **reopen** it are:

- **A consumer appears that wants the span schema without OTLP** — a real one,
  named, not hypothetical. That is the beneficiary the crate-count argument
  never had. Note phase 0 may well serve it without any relocation: it can
  implement against the committed artifact.
- **`bitrouter-otel` is wanted as a published product surface**, adopted
  without the SDK. A positioning decision, needing evidence this document did
  not gather; do not let it borrow *The cloud question*'s arguments, which are
  about internal dependency shape.
- **The build-cache measurement changes materially** — `apps/bitrouter` stops
  dominating rebuild time, or OTel starts shipping its crates out of lockstep
  so an SDK-only patch becomes the common case. Re-run the timings; do not cite
  the ones above as still current.
- **`opentelemetry` 1.0 lands and stabilises traces.** Much of the churn
  argument weakens; re-evaluate rather than execute this out of momentum. As of
  writing this has not fired — crates.io max_version is 0.32.0 with no rc.
  (With D1 withdrawn this now cuts the other way: 1.0 would weaken the case for
  reopening, not strengthen the case for proceeding.)

## Follow-ups

- ~~Add a bullet for this file to [`docs/README.md`](README.md).~~ Done in
  phase 0.
- The prototype from which D1/D3's verification came is scratch-only and is not
  proposed for the tree; phase 2 should port it into `http_layer.rs` behind a
  feature so both paths can be compared. Note the mechanism does not exist yet:
  `apps/bitrouter/Cargo.toml` has no `[features]` section and pins the SDK's
  feature list at `:20`, so "behind a feature" requires adding a pass-through
  feature to the binary crate first, or running the comparison from an SDK-level
  test instead.
- `docs/CLI.md:40,46` describe the `RUST_LOG` → SERVER-span coupling that D3
  removes. They are in phase 2's scope, not a follow-up — listed here only so
  the lockstep requirement is not lost if phase 2 is descoped.
- ~~Answer D1's cloud question in writing before phase 3 is scheduled. It is
  the one open item that can invalidate the whole migration, and it is not
  resolvable inside this repo.~~ **Done — and it did invalidate the migration.**
  Answered as option 3 in *The cloud question*; phase 3 is withdrawn. It turned
  out to be resolvable inside this repo after all: the deciding evidence was a
  consumer ledger and a rebuild-timing run, not a negotiation with cloud.
- **Open, and now the largest OTel item on the board: cloud's migration off
  `bitrouter-observe`.** It pins a crate that no longer exists in this tree,
  and imports the exporter, the config types and `SpanAttributes` from it. This
  document does not own that work, does not block on it, and does not reduce it
  by withdrawing phases 3–4. Someone should schedule it against
  `bitrouter-sdk`'s `otel` module.
