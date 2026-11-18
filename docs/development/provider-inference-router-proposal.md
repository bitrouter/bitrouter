# BitRouter Provider Inference Router Implementation Spec

Status: proposed implementation contract and sequential implementation guide. The architecture defines the intended module boundaries; implementation choices marked provisional must be resolved in Phase 1 before they become public configuration. No functionality described as a deliverable is claimed to exist already.

## Goal

Make the BitRouter gateway independently usable, with independently optional semantic routing and provider inference scheduling. Implement an opt-in provider scheduler that combines bounded fair queueing, local dispatch reservations, observed provider performance, and approximate prefix affinity while preserving gateway authorization, protocol fidelity, streaming, fallback, and receipt semantics.

The scheduler chooses a provider destination for an allowed model. Providers retain all replica selection, batching, KV storage, prefill, and decode scheduling responsibilities.

Completion requires working, tested compositions of gateway alone, gateway with semantic routing, gateway with inference routing, and gateway with both. Also demonstrate a user-side semantic gateway forwarding a concrete model to a serving-side inference gateway using two independent processes. All decisions must be explainable without exposing prompts, credentials, or private adapter claims.

## Baseline and document ownership

- Implementation reference: [BitRouter OSS PR 1031](https://github.com/bitrouter/bitrouter/pull/1031), inspected head `f19fd8f790b09c52ab5945654a55b2467c3d8ec1`.
- This documentation-only proposal targets `main`; it does not bring the PR 1031 implementation stack into `main`. Before implementation, verify that the chosen base contains that stack or reconcile its reviewed successor. Do not assume these execution contracts already exist on the documentation branch.
- This proposal is tracked at the user's explicit request for implementation review. It is a scoped exception to the working-material guidance in [the documentation policy](../DEVELOPMENT.md#working-material), not a new public wire-format specification or a description of shipped behavior.
- Keep subsequent progress ledgers, raw measurements, and agent handoffs in implementation PRs, CI artifacts, or untracked local work areas. Do not add them to this proposal or shippable skills.
- Update current engineering behavior in existing development guides as it lands. Extend the canonical [configuration specification](../specs/configuration/README.md) or [service specification](../specs/service-api/README.md) when changing those formats, together with DTOs, schemas, examples, and compatibility cases. Do not create a competing provider-scheduler wire specification.

The implementation remains proposed. Publishing this document does not activate scheduling or change existing gateway behavior.

## Scope and success boundaries

### Required compositions

| Composition | Model decision | Provider decision | Required behavior |
| --- | --- | --- | --- |
| Gateway | Explicit caller model or existing configured mapping | Existing static resolution and fallback | No semantic classifier or adaptive scheduling runtime is required |
| Gateway with semantic router | Existing semantic selection | Existing static resolution and fallback | Semantic selection works without the inference router |
| Gateway with inference router | Concrete model supplied by caller | Adaptive selection among eligible provider destinations | Scheduling works without semantic routing, Core execution, or Harness |
| Gateway with both | Semantic selection first | Scheduling for the selected model | Exactly one final generation execution owner |
| Two gateway processes | User-side semantic decision | Serving-side provider scheduling | Model and protocol constraints survive the network boundary; no competing retry owner |

The term gateway core does not mean the existing Cargo `core` feature. Preserve that feature's current meaning. Do not make the new inference scheduler depend on the native Core scheduler or Harness.

A provider-side deployment with one eligible destination still supports admission, fairness, and waiting; it has no cross-provider placement choice. An account, region, or deployment may be a separate destination when configuration and authority explicitly distinguish it.

### Initial release scope

- Static generation candidates supporting existing Chat Completions, Responses, and Messages paths, including streaming and non-streaming execution.
- One fixed selected model per scheduling cycle; use existing compatibility filtering for provider aliases and protocols.
- In-process scheduling, observations, prefix indexing, and queue ownership. No remote scheduler RPC is required.
- Best-effort process-local affinity and load observations. Hard local concurrency bounds are enforceable; upstream physical capacity is not inferred from them.
- Existing explicit ordered-dispatch inputs remain fixed. Adaptive planning before ordered admission is a distinct operation with the authority requirements below.
- Existing unsupported operations retain their previous behavior unless scheduling was explicitly required for them, in which case reject clearly before a provider call. Do not silently claim scheduling support for embeddings, counting, continuations, OAuth continuations, or native Harness roots.

### Deferred work

Replica routing, token batch scheduling, prefill/decode disaggregation, KV transfer, GPU memory accounting, engine KV-event ingestion, autoscaling, durable asynchronous jobs, cross-process global quotas, public scheduler plugins/RPC, online learned predictors, speculative generation, hedging, and automatic model substitution are out of scope.

Do not create placeholder implementations or unused extension surfaces for deferred work.

## Architectural contract

### Ownership

The gateway owns authentication, normalization, request checks, destination authorization, provider protocol preparation, actual sends, stream delivery, fallback permission, and required settlement/receipts. Its concrete execution protections remain effective with either router disabled.

The semantic router owns task interpretation, model suitability, and already-authorized context selection. It returns an explicit model decision. If selection itself uses inference, use a dedicated route with explicit budget and a semantic-selection bypass to prevent recursive selection.

The inference router owns a bounded queue, scheduling reservations, provider observations, approximate prefix state, and selection policy. It receives only eligible candidate handles. It cannot add a destination, widen authority, retrieve secrets, mutate the selected model or prompt, or grant retries.

Use application-owned modules first. A proposed layout is `gateway::scheduling::{candidate, state, prefix, policy, queue, observation}`. Names are internal design suggestions, not a requirement to create six files before they have consumers. Preserve existing `gateway::routing` semantic behavior and gateway pipeline ownership. Shared AI contracts remain runtime-free; the SDK remains a transport client.

### Decision sequence

1. Authenticate and normalize identity, deadlines, and applicable policy.
2. Apply existing defaults, entry checks, and permitted semantic selection in their existing order.
3. Resolve authorized provider candidates and protocol/capability compatibility.
4. Freeze model, prompt, policy generation, authority references, and candidate membership for this request.
5. Admit to the bounded scheduler queue if configured. Queue admission is not provider dispatch permission.
6. Choose dispatchable work, refresh advisory signals, and select a destination within the frozen membership.
7. Atomically book local scheduling capacity; rerun existing live authority/rate/health checks and receipt gates at the actual-send boundary. Roll back unsent scheduling capacity on failure.
8. Let the gateway execute. Feed typed send, response, progress, and terminal events back to scheduling state.
9. If existing execution rules permit fallback, select only from the remaining permitted sequence and acquire a fresh local reservation. Never reset the total deadline.

No mutex may be held across provider I/O, SQL, or an await. Scheduler admission must integrate with the existing authority lock order without inversion. Use owned cancellation and terminal cleanup, not detached tasks.

### Candidate identities

Each candidate retains its existing immutable execution identity and authority references. Scheduling uses nonsecret handles. Distinguish:

- Destination identity: configured provider, model/version, endpoint/deployment, region/account when explicit, and relevant configuration generation.
- Quota identity: the existing authorized shared rate group or credential scope. Multiple destinations may share it.
- Health identity: existing profile-local or shared health group. Do not create a contradictory second circuit breaker.
- Cache identity: model compatibility, provider-defined account/deployment cache domain, tenant isolation, adapter projection version, and relevant cache controls.
- Metrics identity: destination plus bounded request shape and response mode.

Do not collapse these into one provider-name key. Credential secrets never appear in identifiers. Equivalent public model names do not establish identical rendering, weights, cache scope, or continuation compatibility.

### Ordered execution and hosted authority

The existing ordered contract accepts a fixed candidate sequence. Actual attempts may form only the permitted ordered subsequence. Adaptive scoring must never reorder an accepted selection or add credentials to its closure.

Implement two explicit modes:

| Mode | May reorder? | May wait? |
| --- | --- | --- |
| Ordinary configured adaptive routing | Before freezing the attempt sequence, within authorized compatible candidates | Within configured queue/deadline bounds |
| Caller-supplied ordered dispatch | No | Only if scheduler policy applies, without resetting existing deadlines or weakening receipt obligations |

For hosted adaptive ordered execution, an authorized planner may rank a candidate manifest before submitting the existing ordered selection. Manifest inspection remains operator-authorized. Ordinary inference credentials cannot enumerate it. Do not internally fabricate caller approval, auto-upgrade v1 to v2, or bypass profiled-route protection. Phase 1 must choose and document the real integration path; if a new service message is necessary, extend the canonical service contract before implementing that path.

The planner's result is advisory until gateway admission. It must retain exact route/configuration revisions and candidate ordinals. Authority changes can invalidate the plan. Unknown transport/body/stream outcomes, cancellation, completed work, and unauthenticated private-adapter outcomes still stop fallback.

## Scheduling state and lifecycle

### Local reservation

A local reservation represents permission to consume this scheduler's configured concurrency, not upstream acceptance, GPU ownership, a monetary reservation, or execution authority. Booking and projected-load increments must be atomic within one scheduler instance. Read-only inspection never books capacity or warms a prefix.

Use one single-owner attempt lifecycle with idempotent terminal handling:

| State | Meaning | Scheduling effects |
| --- | --- | --- |
| Queued | Accepted into bounded local waiting storage | Charge queue bytes/count; no destination capacity |
| Reserved | Destination chosen and local capacity booked | Charge destination/shared local limits; add tentative affinity |
| Sent | Gateway crossed its actual-send boundary | Record recent exposure; retain outstanding work |
| Output observed | First useful model output observed | Move from awaiting output to streaming/completing estimate |
| Terminal known | Gateway classified completion or definitive rejection | Close reservation once; update typed outcome measurements |
| Outcome unknown | Local transport ended without known provider termination | Stop fallback; retain uncertainty independently of socket lifetime |

Unsent cancellation releases the reservation and removes its tentative affinity. Cancellation after send does not prove the provider stopped. Separate locally active connections from possibly outstanding provider work. Retain a bounded uncertainty record/charge until explicit terminal evidence or a configured conservative expiry; expiry is an estimate, not proof of termination. A full uncertainty budget must backpressure or reject new work, not silently erase it. Unknown receipts remain unknown after scheduler expiry or restart.

Streaming reservations survive response headers and the first event. A role-only chunk, keepalive, empty delta, or HTTP 200 does not establish useful output or successful terminal. Non-streaming completion latency must not be mislabeled TTFT. Reasoning/tool output must be classified consistently with existing protocol observations; document the metric definition.

### Queueing and fairness

- Bound total and per-tenant request counts, retained bytes, active reservations, and scheduling identities. Account actual retained payload/copy sizes; do not assume queue count bounds memory.
- The scheduling deadline includes queue wait, attempts, and fallback. Use monotonic time internally and retain stricter inherited execution deadlines. An optional queue timeout may be shorter, never longer in effect.
- Derive tenant and service-class identity from authenticated policy. Client hints do not grant priority.
- Start with weighted deficit round robin across authorized tenant/class flows and FIFO within each dispatch lane. Select explicit policy weights, with equal weights as the baseline.
- Charge a fixed, bounded estimated work amount when enqueuing. Include input and expected output work with a positive floor; do not subtract uncertain prefix hits. This is fairness accounting, not billing. Validate caller output limits and cap their influence.
- A provider-pinned blocked lane must not stop an independent dispatchable lane. For flexible requests, use bounded scanning or eligibility lanes so a blocked head cannot indefinitely stall unrelated capacity; include aging/progress guarantees for skipped requests. Bound scans per dispatch tick and wake on relevant state changes.
- Do not reserve several destinations while waiting. Recompute advisory selection immediately before booking.
- Global and per-flow saturation fail promptly with existing appropriate local error mapping. Reuse established semantics for 429, deadline errors, and 503; document any new public reason in the canonical service API.
- Cancel queued requests promptly and join cleanup at shutdown. In-memory queues do not survive restart and must not be advertised as durable jobs.

## Approximate prefix affinity

### Projection and matching

Implement a deterministic, versioned, adapter-owned projection of model input for each eligible candidate. Include ordered system instructions, tools, messages, content boundaries, and cache-relevant controls. Preserve distinctions that affect model input. Exclude credentials, request identifiers, and transport-only fields. The projection is neither the serialized HTTP request nor a claim about the provider's exact token stream.

Use fixed-size UTF-8 byte blocks and chained keyed cryptographic digests. The previous digest and a domain/version are included in each next digest. Store only complete blocks and require a contiguous prefix match. Lengths are bytes; optional token estimates must remain explicitly estimated. A provisional block size is 256 bytes, to be measured in Phase 8 rather than treated as an engine block size.

Use an ephemeral process key initially, with distinct cache namespaces and no hash export. Digest collisions must never authorize execution or alter prompt content. Treat fingerprints as sensitive derived data. Do not use raw prompt strings as map keys or logs.

Default cache isolation is tenant plus provider cache domain and model/rendering compatibility. A configured shared scope requires explicit operator policy and must not make another tenant's prompts or fingerprints observable. For v1, cross-tenant sharing may remain disabled entirely.

Opaque continuation handles and unsupported multimodal/provider fields must disable affinity for the affected candidate or restrict matching to a proven safe leading prefix. Never serialize unknown input away and claim a full match. Disabling affinity must not change otherwise valid execution semantics.

### Evidence and retention

Keep independent evidence categories:

1. Tentative placement from a successfully booked local reservation, with short expiry and rollback before send.
2. Recent exposure from an actual send, with age decay and lower confidence for unknown outcomes.
3. Successful generation exposure, still not proof of retention.
4. Provider-reported cached-input usage for a particular request, retained as measurement rather than invented exact block residency.

Concurrent reservations with the same prefix need independent ownership/reference accounting; canceling one must not erase another's evidence. Prevent self-credit: the current request must not improve its own score by inserting its reservation before selection.

TTL/decay bounds belief about upstream retention. LRU bounds local memory independently. Cache expiry cannot free a live reservation or change receipts. Different provider cache TTLs may be configured; do not infer them from index capacity. Restart starts cold safely.

Start with tentative affinity disabled in policy scoring until its burst tests pass. Once enabled, its credit must be smaller than successful recent-exposure credit and must compete with the load added by the reservation. No assumption of concurrent prefix coalescing is permitted.

## Provider selection policy

### First implemented objective

Ship explicit static, load-aware, session-affinity baseline, and prefix-aware comparison modes for validation. Only expose user-facing modes that have a maintained configuration consumer. The initial adaptive product objective is latency preference subject to authorization, compatibility, configured quota/concurrency, and any already-enforced budget limits. Do not claim monetary optimization from `routing_weight`.

For initial selection, use deterministic normalized pressure with bounded affinity credit:

1. Exclude candidates that cannot currently book the relevant local scheduling limits. Existing authority/rate/health checks remain mandatory and final.
2. Compute pressure from outstanding local work relative to configured destination capacity. Include reserved requests and a bounded request-size estimate; distinguish unknown outstanding work.
3. Apply a bounded affinity discount using matched-prefix fraction, freshness, and evidence confidence. Cap the discount so it cannot overwhelm a materially less-loaded candidate. Phase 1 freezes the exact formula, units, caps, and tie rules in implementation tests.
4. Use request-bucketed observed latency as an additional bounded preference only when sufficiently fresh samples exist. Missing samples are unknown, not zero or infinite performance. Use deterministic configured order as the cold-start/tie fallback.
5. Select within the frozen authorized set and atomically book. A lost booking race triggers bounded reconsideration, not an unbounded spin.

This is a heuristic score, not a duration prediction. Do not add a separate cache bonus to a future latency predictor already conditioned on cache affinity. Future monetary policy must use reliable price units and cached/uncached tariffs rather than repurposing relative weights.

Use deterministic tie-breaking for reproducibility. If exploration is later enabled, it must be bounded, seeded/testable, and confined to eligible candidates within policy limits. Do not add it before an acceptance case needs it.

### Observation rules

Track local queue wait, scheduling time, send-to-first-useful-output where observable, end-to-end duration, outcomes, provider-reported usage/cached usage, and locally outstanding work separately. Bucket by destination, model, streaming mode, and bounded input/output-size categories. Bound samples and cardinality, age out stale measurements, and make missing values explicit.

Observe failures and timeouts as well as successes so fast successful samples do not hide a provider with frequent stalls. Reuse existing health classification; timeout data may inform ranking but never grant fallback. A disconnect does not establish a zero-duration or zero-token sample.

Do not assume our traffic is the provider's total load. Do not interpret stream chunks as token counts. Never convert estimated usage or cache exposure into billing evidence.

## Configuration and compatibility requirements

Phase 1 must define the concrete configuration shape in the existing configuration object, rather than implementing provisional names from this handoff blindly. The following semantics are mandatory:

- Scheduling is disabled by default; existing configurations retain their routing behavior.
- Runtime activation and build availability are distinct. Missing enabled feature support fails startup/reload before traffic; it does not silently fall back to static mode.
- Semantic routing and inference routing are independently buildable. Provisional feature names are `semantic-routing` and `inference-routing`; avoid renaming `core`. Preserve existing distribution behavior unless a separately justified compatibility change is needed.
- Enabling inference routing requires finite queue count/byte budgets, queue wait limit, per-destination concurrency, state/index budgets, and uncertainty retention bounds. Defaults must have documented units and measured resource implications.
- Distinguish omitted, null, zero, and invalid values. Reject invalid weights, nonfinite scores, inconsistent ranges, and ambiguous destination scopes.
- Hot reload creates a new policy generation. Queued requests retain their policy/candidate binding; current revocations still apply. New requests use the new generation. Existing reservations drain under original ownership without resetting shared safety counters.
- A single process owns each configured local quota budget. Multiple independent processes cannot claim a global limit; fail clearly for unsupported hard-global settings or require explicit partitioned budgets.
- Scheduling internals do not become arbitrary provider request fields. New headers/DTOs require canonical service-contract review; existing private adapter signatures and scrubbing remain intact.

Keep compiler/runtime state in the application. Only genuinely shared wire/configuration contracts belong in `bitrouter-ai`. Do not introduce unused SDK calls, alternate configuration stores, or a second authoritative admission system.

## Sequential implementation phases

Each phase must leave a buildable, reviewable change. Do not mark a phase complete based only on compilation when its behavioral gate requires execution. Continue to the next phase after its checks pass; ask for clarification only when a genuine unresolved product/authority decision prevents safe implementation.

### Phase 0 Establish the implementation base

Goal: work against the intended OSS stack with a verified map of existing ownership.

1. Read repository `AGENTS.md`, `README.md`, documentation policy, architecture, gateway, tenant-routing, and tenant-service guides at the implementation base.
2. Inspect PR 1031's head and use an isolated `codex/` branch/worktree based on the agreed stack. Preserve unrelated working changes.
3. Trace model selection, candidate resolution, checks, actual-send admission, ordered intent, receipts, cancellation, streaming terminal, and finalization. Record code pointers in the PR.
4. Inspect existing feature-boundary and fixture tooling. Reuse it rather than inventing parallel infrastructure.

Gate: exact base recorded; every required execution boundary has a source owner; no accidental reliance on the older main checkout.

### Phase 1 Freeze module and configuration contracts

Goal: remove integration ambiguity before implementing stateful scheduling.

1. Define the minimal selector, scheduler reservation, and typed observation interfaces with actual planned consumers.
2. Finalize independent feature gating and all four composition paths, including semantic auxiliary inference without recursion.
3. Choose the hosted pre-admission planner path and preserve existing ordered input semantics. List supported and rejected operation roots.
4. Define concrete configuration DTOs and schema behavior, default-off activation, score formula, local limits, key scopes, lifecycle outcomes, deadline rules, and reload generations.
5. Update the existing canonical configuration/service specs only where their objects change; include compatibility cases for old/new readers and writers, rollback, omitted fields, and missing features. Mark unimplemented additions as proposed until implemented.

Gate: one coherent design covers ordinary and hosted execution; no extension bypasses execution authority; each public format has a single owning specification and implementation consumer. Review this concrete contract in the implementation PR before treating it as stable.

### Phase 2 Make gateway and semantic routing independent

Goal: gateway-only and gateway-plus-semantic compositions work without the new scheduler.

1. Refactor existing semantic implementation behind its optional assembly/build boundary without changing current model decisions.
2. Keep static routing, protocol adaptation, execution protections, observations needed for execution, and configured fallback in the gateway.
3. Add the scheduling integration seam with an immediate/default execution path that preserves legacy behavior; do not start queue workers when disabled.
4. Verify minimal gateway and semantic-enabled builds and representative request checks/stream behavior.

Gate: disabled inference routing is behaviorally equivalent for the covered static/semantic paths; no semantic classifier runs in gateway-only mode; configured unavailable features fail explicitly.

### Phase 3 Implement local reservations and observations

Goal: concurrent selection sees already-assigned work and all reservations have bounded cleanup.

1. Implement candidate/state identities and atomic local booking with single-owner terminal cleanup.
2. Wire actual-send and stream-terminal events from the existing executor. Keep successful headers separate from completion.
3. Implement unknown-outcome accounting, unsent rollback, cancellation, shutdown, reload, and bounded uncertainty retention.
4. Add bounded observations and read-only decision inspection that has no selection side effects.

Gate: concurrent attempts never exceed configured local active bounds; failures at every handoff clean up exactly once; unknown provider work remains distinguishable from completed work; no locks span I/O.

### Phase 4 Implement bounded fair queueing

Goal: decide when work runs without provider-specific head-of-line starvation.

1. Implement count/byte bounds, deadlines, cancellation, and weighted tenant/class fairness.
2. Implement dispatchability lanes or bounded scanning with aging; preserve progress for large requests and pinned providers.
3. Reuse the existing live admission gates. Do not charge RPM by polling or refund profiled RPM contrary to its existing rules. Any advisory availability check must remain side-effect-free.
4. Drain safely at shutdown and expose bounded local rejection/wait reasons.

Gate: provider A saturation does not idle usable B capacity indefinitely; tenant weights have measurable effects; queue bytes stay bounded; expired/cancelled work never sends; no busy polling.

### Phase 5 Implement approximate prefix state

Goal: estimate prefix affinity without tokenizer services, engine telemetry, or prompt retention.

1. Implement versioned adapter projections and chained digests with tenant/model/cache-domain isolation.
2. Implement complete-block longest-prefix lookup, TTL/decay, LRU memory caps, and explicit unsupported-input handling.
3. Wire reservation, send, completion, rejection, and cached-usage observations to separate evidence categories.
4. Test concurrent sibling requests, rollback, expiry, restart, model/adapter changes, and cross-tenant separation.

Gate: no raw prompt or credential retention in the index; known mismatches do not create hits; expiry does not affect dispatch authority; query-only inspection cannot manufacture affinity.

### Phase 6 Integrate provider policy and ordered planning

Goal: select providers using the frozen policy with bounded load and affinity effects.

1. Implement the Phase 1 score and cold-start rules, with a load-only baseline and optional prefix contribution.
2. Refresh signals at dispatch; make booking races and stale configuration deterministic and bounded.
3. Freeze the chosen attempt sequence before execution admission where required. Integrate hosted planning only through its Phase 1 authorized path.
4. Preserve exact caller-supplied order, per-hop admission, private-adapter proof checks, and unknown-outcome stop conditions.

Gate: warmer destinations win only when the policy permits; overloaded warm destinations lose to viable alternatives; no model substitution or unauthorized target appears; explicit ordered inputs are unchanged.

### Phase 7 Verify deployment compositions and operator behavior

Goal: make the design usable in one process or in the two-gateway topology.

1. Exercise all four build/runtime compositions with JSON and streaming requests.
2. Run two real gateway processes against Cargo-built provider fixtures. Semantic selection occurs on the user side and provider selection occurs on the serving side.
3. Verify model/constraint fidelity, remaining deadline propagation, cancellation, no retry multiplication, and no internal metadata leakage to providers or public diagnostics.
4. Update applicable CLI/skill/configuration/operator documentation and plugin manifests in lockstep when their surfaces change. Keep the skill under its repository length constraint.

Gate: one reproducible local command/test path demonstrates each topology; no hidden dependency on Core/Harness; reload and feature-unavailable behavior are tested; documentation matches executable behavior.

### Phase 8 Validate effectiveness and finish the implementation

Goal: demonstrate safety and useful routing behavior without conflating synthetic results with provider guarantees.

1. Run the acceptance matrix below with deterministic providers and replayable workload seeds.
2. Compare static/load-aware, session-stickiness, and prefix-aware policies. Include warm, cold, skewed, throttled, and affinity-defeating providers.
3. Measure router CPU/memory, scheduling latency, end-to-end latency, queue wait, fairness, success/timeout rates, fixture/provider cached usage, and cost when reliable tariffs exist.
4. Run required repository tests, Clippy, formatting, schema checks, and the reduced feature matrix. Inspect failures; do not weaken assertions or silence checks.
5. Publish concise implementation and validation results in the PR, with exact commits, commands, environment, and limitations. Keep raw evidence untracked or in CI artifacts.

Gate: every required case passes; the final completion checklist is satisfied. Live paid-provider experiments are a separate, explicitly budgeted acceptance step and are not required to claim the locally verified implementation complete. Without them, do not claim real-provider savings or production readiness.

## Behavioral acceptance matrix

| Area | Required case and observable result |
| --- | --- |
| Default compatibility | Scheduling omitted/disabled preserves configured resolution, entry checks, protocol behavior, and fallback |
| Feature isolation | Each composition builds and serves; unsupported enabled features fail before provider work |
| Authority | Revocation while queued or reserved prevents a new send; cached selection never substitutes for live grants |
| Model fidelity | Semantic model decision survives provider scheduling and two-gateway forwarding |
| Explicit order | Supplied ordinals remain unchanged; only permitted ordered-subsequence fallback occurs |
| Ambiguity | Timeout after send, truncated 200/SSE, or missing adapter proof stops replay/fallback |
| Capacity race | Concurrent selection books at most configured local capacity, including reservations not yet sent |
| Streaming | Headers/keepalive/empty chunks do not release capacity; terminal/drop paths close exactly once |
| Queue bounds | Count and byte ceilings hold under oversized requests and many tenant identities |
| Cancellation | Queued cancellation never sends; unsent reservation rolls back; sent cancellation remains uncertain |
| Fairness | Equal and unequal weights behave as configured; long output estimates have a positive charge; large requests progress |
| Head-of-line blocking | An A-only blocked request cannot indefinitely block a B-eligible request |
| Cache identity | Tenant, model, cache domain, rendering version, and tool/system-prefix changes isolate matches |
| Prefix matching | Only contiguous complete-block prefixes match; changed early blocks invalidate later matches |
| Evidence | Inspection does not warm; reservation rollback removes only its own tentative state; cached-usage absence remains unknown |
| Expiry and pressure | TTL expires belief; LRU caps memory; neither evicts live execution ownership |
| Load escape | Busy warm A loses to usable B under the frozen affinity cap |
| No cache benefit | A provider with no cache, random internal placement, or eviction does not cause unbounded sticky concentration |
| Cold start | Unknown metrics and empty indexes produce deterministic valid choices, never zero-latency claims |
| Reload | New generations do not reset active shared limits or change old request bindings; current revocations still apply |
| Shutdown and restart | Cleanup is joined; queue loss is explicit; no completed/unknown execution is replayed from scheduler state |
| Private adapter | Exact attempt binding, authenticated outcomes, stream validation, and diagnostic scrubbing remain intact |
| Observation privacy | No prompt, fingerprint, secret, private claim, or unbounded caller label appears in exported diagnostics |

## Measurement protocol

Use a Cargo-built fixture with independently configurable latency, concurrency, rate rejection, cache TTL, cache-hit benefit, internal random placement, stream behavior, and cancellation acknowledgment. Synthetic provider cache counters represent fixture truth only.

Run the same seeded workload and offered arrival rate for each policy. Include iterative agent turns, shared system/tool prefixes, divergent suffixes, cold prompts, long output, unequal tenant traffic, burst arrivals, and provider failures. Use fresh router/provider state for cold runs and explicitly controlled warmup for warm runs. Report repetitions and variability, not a single best run.

Phase 1 must set the numeric latency/resource budgets and Phase 8 must report them before declaring performance acceptance. Suggested provisional targets are no more than 5 ms p95 scheduling work for 64 candidates and a 32 KiB text prompt in a release build on recorded hardware, and no more than 5 percent p95 end-to-end regression against load-aware routing on the no-cache fixture at equal offered load. These are proposed engineering budgets, not measured properties. Change them only with explicit rationale, never retroactively to hide a failure.

On a controlled warm-cache fixture, prefix routing must improve measured cached-input reuse or latency/cost relative to load-aware routing without violating capacity, fairness, or success-rate gates. Compare against session stickiness too; report if it performs equally well or better. That result may justify keeping prefix routing opt-in or choosing a simpler default. Do not tune against undisclosed fixture knowledge that a real provider would not expose.

A replay of historical request shapes cannot establish counterfactual provider cache residency or exact cost savings. Distinguish replayed inputs, simulated provider behavior, and live observations in results.

## Agent execution rules

- Follow all repository instructions. No `#[allow(...)]` check bypasses, public forwarding re-exports, panic-producing `.unwrap`/`.expect`/`panic!`, unused abstractions, or executable Python fixtures.
- Use conventional commit/PR titles under 60 characters where applicable.
- For source changes run `cargo nextest run --all-features`, or `cargo test --all-features` when nextest is absent; run `cargo clippy --all-features` and `cargo fmt -- --check`. Run doctests separately when nextest does not cover them and the existing workspace checks require them.
- Add the feature-composition checks selected in Phase 1. Do not treat all-features success as evidence that reduced builds work.
- Use meaningful race, lifecycle, protocol, and integration tests. Reuse deterministic time/fixtures where possible; avoid sleep-based correctness assertions.
- Changes to flags, environment variables, defaults, or wiring require the BitRouter skill and applicable agent-plugin manifests to remain accurate in the same change. This proposal is not a shippable skill.
- Do not deploy, enable production traffic, or run paid experiments just to complete a validation checklist.
- At phase handoff, report completed behavior, exact base/head, relevant files, tests and results, unresolved concerns, and the next phase. Keep this in the PR or untracked work area rather than a tracked progress ledger.

## Final completion checklist

- [ ] Gateway-only operation has no required semantic or inference scheduling runtime.
- [ ] Semantic routing and inference routing are independently optional and compose together.
- [ ] The two-process user-side/serving-side topology passes actual JSON and streaming tests.
- [ ] Scheduling stays at the provider boundary and never substitutes an unauthorized model or destination.
- [ ] Queueing, reservations, cancellation, uncertainty, and reload satisfy the lifecycle contract.
- [ ] Approximate affinity has bounded memory, explicit scope/freshness, and separate tentative/exposure/usage evidence.
- [ ] Existing ordered dispatch, private-adapter verification, receipts, and fallback barriers remain intact.
- [ ] Required acceptance cases, feature builds, repository checks, and measurement budgets pass.
- [ ] Operator-visible behavior, schemas, compatibility cases, skills, and manifests match implementation.
- [ ] Results distinguish local correctness, synthetic effectiveness, and any separately authorized live-provider evidence.

## Source map and design references

At the implementation base, start with these source owners:

| Concern | Repository path |
| --- | --- |
| Assembly and feature composition | `apps/bitrouter/src/assemble.rs`, `apps/bitrouter/Cargo.toml` |
| Model selection and candidate chains | `apps/bitrouter/src/gateway/pipeline/routing.rs`, `apps/bitrouter/src/config/routing_table.rs` |
| Semantic model/context planning | `apps/bitrouter/src/gateway/routing/plan.rs`, `apps/bitrouter/src/gateway/routing/preparation.rs` |
| Pipeline integration | `apps/bitrouter/src/gateway/pipeline/builder.rs`, `execution.rs`, `hooks.rs` |
| Actual send and streaming | `apps/bitrouter/src/gateway/pipeline/executor.rs`, `executor/tenancy.rs`, `executor/stream_bridge.rs` |
| Tenant runtime controls | `apps/bitrouter/src/tenancy/routing.rs`, `operational.rs`, `operational/state.rs` |
| Ordered execution and private adapter | `apps/bitrouter/src/tenancy/ordered.rs`, `execution.rs`, `execution/private_adapter.rs` |
| Existing contracts | `docs/specs/configuration/README.md`, `docs/specs/service-api/README.md` |

The following external references inform design choices; they do not define BitRouter behavior or promise cross-provider performance:

- [llm-d router](https://llm-d.ai/docs/architecture/router): proxy/decision separation.
- [llm-d approximate prefix routing](https://llm-d.ai/docs/architecture/kv-management/prefix-cache-aware-routing): tokenizer-free block approximation and chained hashes.
- [llm-d flow control](https://llm-d.ai/docs/architecture/router/flow-control): separate flow fairness, ordering, and capacity control. BitRouter does not inherit strict priority or replica saturation assumptions.
- [Dynamo routing concepts](https://docs.dynamo.nvidia.com/dynamo/v1.6.1/knowledge-base/modular-components/router/routing-concepts): combined cache credit and assigned work. BitRouter does not copy GPU-block cost units.
- [Dynamo selection service](https://docs.dynamo.nvidia.com/dynamo/v1.6.1/knowledge-base/modular-components/router/standalone-selection): advisory selection versus booking, lifecycle, and local/distributed consistency limits.
- [Dynamo retention and tuning](https://docs.dynamo.nvidia.com/dynamo/v1.6.1/knowledge-base/modular-components/router/configuration-and-tuning): approximate TTL and speculative state. BitRouter does not simulate unobserved physical cache capacity.
- [Dynamo deficit round robin](https://docs.dynamo.nvidia.com/dynamo/v1.6.1/knowledge-base/modular-components/router/deficit-round-robin): weighted service and dispatchability. BitRouter uses its own conservative fairness charge.
- [Dynamo custom policies](https://docs.dynamo.nvidia.com/dynamo/v1.6.1/knowledge-base/modular-components/router/write-custom-routing-strategies): policy ranking separated from host-owned eligibility and reservations.

## Starting instruction for an implementation agent

Read this proposal and the repository instructions. Work from the verified PR 1031 baseline or its reviewed successor. Implement Phases 0 through 8 sequentially, keeping each phase buildable and validating its behavioral gate before proceeding. Preserve existing execution authority and ordered-dispatch contracts. Do not expand into replica scheduling, distributed quotas, paid experiments, or deployment. Completion means the goal and final checklist above are satisfied, with exact test and measurement evidence in the implementation PR.
