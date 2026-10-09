# Decisions API acceptance evidence

Contract: [DECISIONS_API_SPEC.md](DECISIONS_API_SPEC.md). Evidence is local to
`codex/decisions-api`, stacked on #962 at
`529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16`. This ledger records the scope of
proof rather than inferring provider or deployed Cloud support from unit tests.

Audit date: 2026-10-07. Local implementation and artifact guards are verified;
the final source state is validated and [draft #965](https://github.com/bitrouter/bitrouter/pull/965)
is published on #962. Hosted CI passed at `aa378f7c`; A11 is now verified
for two bounded API-key calls through an isolated local gateway. The scope and
redacted evidence are recorded below.

| Criterion | Authoritative evidence | Scope / qualification |
| --- | --- | --- |
| A1: native text/image input | [AI native tests](../crates/bitrouter-ai/tests/decisions.rs): native round-trip, nullable fields, unsupported input/privacy, image budget; [SDK gateway tests](../crates/bitrouter-sdk/tests/decisions.rs): projected model and preserved wire, unsupported input before dispatch | Synthetic text/images; inline-only image validation, ordered detail and fields retained |
| A2: typed answers/refusals | AI native round-trip and `refusals_and_invalid_answers_keep_independent_usage_evidence`; SDK completed-invalid-output test; [app accounting](../apps/bitrouter/src/metering/decisions_tests.rs) covers refusal and malformed outputs | Predicate/choice/score validation and positional names/order; boolean/string identity; no synthetic generation result |
| A3: direct invocation/auth | AI selected-model projection/source immutability, bounded selected-account 401 recovery, credential redaction and cancellation tests; admitted native I/O cancellation/timeout fixture | Controlled HTTP, no real OpenAI account; same shared selected-target transport |
| A4: hard operation routing | SDK operation/pin/mixed-protocol tests and default generation-host rejection; AI generation/decision mismatch checks | Operation filtering before protocol preference and again after route hooks; custom executor native output also validated |
| A5: required protections | SDK builder/global and bound-router checker requirements; app gateway auth/ACL/key/policy expiry/spend/rate cases; [judge reservation test](../apps/bitrouter/src/evolution/costs.rs) preserves namespace on native rejection | Actual app hook implementations over SQLite; independent registration requirements; generation-only defaults/tools/continuation retain scope |
| A6: price/usage agreement | App native HTTP/gateway/SQLite tariff test; [eval shared-tariff test](../apps/bitrouter/src/eval/settlement.rs); SDK protocol/frozen/unknown stream pricing tests; cache-gated export and authoritative correction | Exact independent overrides, missing buckets preserved, actual target match; native nonzero cache remains unavailable |
| A7: disconnect/shutdown | `observed_native_disconnect_and_shutdown_preserve_sqlite_settlement` uses a real TCP gateway, held HTTP upstream, gateway-side cancelled-handler signal, shutdown join and one persisted row | Explicit upstream admission and closed client socket; no sleeps or cancellation-status-only proof; synthetic upstream |
| A8: malformed completion/fallback | SDK malformed HTTP and inconsistent custom typed-result fixtures retain independently decoded usage, fail delivery and bypass retry-all policy; app invalid-output cost row; pre-completion status fallback test | One native attempt settles once; wrong-operation generation usage is not native accounting evidence; generation charging is preserved |
| A9: generation/boundaries | Full workspace all-feature tests and doctests; CI feature matrix and positive-backed dependency tree guards | Generation conversion, tools, Responses continuation, auth, evolution, IPC and telemetry regressions; local and hosted evidence |
| A10: representations/artifacts/legacy | Registry profile/key/append mechanism test; app registry mapping and alias assembly tests; threshold/profile/frozen/legacy tests; reload tariff/profile/known-price classifier; generated catalog/schema and pinned SDK public API guards | Global and US/EU matching profiles, 272000/272001 whole-input boundaries; old rows remain legacy; source and embedded catalogs rebuilt |
| A11: credentialed gateway call | [Live evidence](DECISIONS_API_LIVE_EVIDENCE.json), 2026-10-07 17:33 UTC: two HTTP 200 native OpenAI calls through the local gateway, preserving typed text/image answers and matching SQLite usage/tariffs | User-supplied temporary API key, model `gpt-6-luna`, immutable PR catalog; downstream loopback auth skipped; zero-cache usage only. No subscription, regional, invoice or deployed Cloud claim |
| A12: native request checks | SDK operation declaration/preparation rejection and fragment projection/cap cases; evidence/name/instructions, boolean/string choices, descriptions and ordered score rubrics | Images reported as excluded coverage; safety ID excluded; bounded fail-closed entry-request scope |

## Local validation

- Full all-feature workspace nextest: **3769 passed, 22 existing skips**, run
  `258bdf17-8ab9-4c7d-ac79-8f5a0d7837b1`. This includes the final admitted direct
  native cancellation/timeout fixture and panic-free fixture migration.
- The expanded ordered score-rubric projection assertion passed separately after
  that run, `6796daa9-5977-46c2-9dfb-fd34b6fae5f1`. It preserves every earlier
  boolean/string, image-exclusion, safety-ID and fail-closed assertion.
- Workspace doctests: **6 passed, 1 ignored**.
- Strict workspace all-feature Clippy including tests, formatting and dist
  freshness: passed after the final score-rubric assertion. No Rust source or
  fixture changes followed those checks.
- Native/pricing/guard/telemetry selected subset: **137 passed**, run
  `ce40dcc1-41bc-43cd-957d-bf9b0bab8100`.
- Registry validate/build, schema generation and `dist-helper check`: passed.
- Pinned public API: nightly `2026-05-05`, cargo-public-api `0.52.0`; positive
  sentinels, native envelopes, unchanged foreign dependency set and no public
  OpenTelemetry types: passed. Source has no added public forwarding exports.
- Feature matrix: AI minimal plus `pkce`, `hosted-login`, `file-store` tests;
  SDK minimal/config/server/ACP; guardrails minimal/SDK; telemetry HTTP/gRPC/server
  checks: passed. Dependency guards verify AI isolation, separate host/guardrail
  trees, SDK/telemetry default leanness and transport separation.
- Resource settings for checks: `CARGO_INCREMENTAL=0`, dev/test debug 0,
  `CARGO_BUILD_JOBS=2`. These change build resource use, not the requested feature
  or test scope. Generated caches were cleaned only in this checkout after disk
  exhaustion; source and other worktrees were preserved.

## Live provider acceptance

The live run began on 2026-10-07 at 17:33 UTC (13:33 America/New_York),
using the short-lived OpenAI API key supplied by the user for this test. The branch binary was rebuilt with all
features from `aa378f7c81777fce3a75d80d926a4d1a57255765`. An isolated loopback
gateway invoked `https://api.openai.com/v1/decisions` with model `gpt-6-luna`.

- Text predicate, boolean choice and score: HTTP 200, **426 input tokens**,
  zero output/cache tokens, **1,657 ms** observed gateway latency.
- Inline image predicate: HTTP 200, **165 input tokens**, zero output/cache
  tokens, **210 ms** observed gateway latency.
- Both correlation headers matched the supplied request IDs. Native answer
  order, names, boolean identity and distributions passed codec validation.
- Graceful shutdown returned exit 0; SQLite contained exactly two matching
  provider-reported usage records, each retaining the `decisions` protocol and
  frozen `openai_global` tariff. Estimates were 43 and 17 micro-USD, **$0.000060
  total**, including existing per-request rounding. These are estimates, not
  verified invoice charges.

[Redacted live response and settlement evidence](DECISIONS_API_LIVE_EVIDENCE.json)
records the tested source, actual model, route and usage. The key was supplied
through hidden stdin to the test process and passed only in the gateway's
environment; the gateway was stopped after the test. Evidence contains no key.

The first attempt used the default registry source on `main`, which still listed
only generation protocols for Luna; local admission returned HTTP 400 before
upstream I/O. The successful run pinned `registry.url` to this PR's immutable
committed catalog. No public catalog/deployment was changed. Until that catalog
is published, deployments need an explicitly compatible registry/configuration.
These two calls establish this account/endpoint/schema path only; measured
latencies are not a benchmark and zero-cache usage does not resolve cache billing.

Hosted [CI run 37572253657](https://github.com/bitrouter/bitrouter/actions/runs/37572253657)
passed all 22 jobs at the tested source head. The live acceptance update changes
only documentation/evidence; it does not change the tested Rust source.

## Catalog and billing interpretation

Only the API-key OpenAI Luna model gets the native protocol declaration. Its
generative order and ordinary prices remain unchanged. The published native
tariff is tagged `openai_global`; configured regional hosts cannot silently
rebind that tariff. Regional deployments provide complete matching rates.

The $0.10/M global input rate and absence of separate cache/output charges are
documented for Decisions. Whole-request long-context and regional multipliers
are combined from the Luna model page. These are documentation-derived estimates,
not invoice evidence. Cache-subset inclusion in the base charge remains
unresolved, so nonzero native cache counters keep cost unavailable. An
authenticated receipt may establish one request's amount without changing this
general gate. [Decisions pricing](https://developers.openai.com/api/docs/guides/decisions#pricing-and-availability),
[Luna pricing](https://developers.openai.com/api/docs/models/gpt-6-luna).

The beta HTTP/typed references were refreshed on 2026-10-07; native user-only
input, image detail/nullability, typed choices, nullable answer names and required
usage counters still match the implementation.
[HTTP create](https://developers.openai.com/api/reference/resources/decisions/methods/create),
[typed resource](https://developers.openai.com/api/reference/typescript/resources/decisions).

## Delivery boundaries

[DECISIONS_API_MIGRATION.md](DECISIONS_API_MIGRATION.md) inventories the alpha
public API migration and the external Cloud checkout. Cloud remains on alpha.30
and its parent SDK migration is separate; no Cloud code/build/deployment was
changed. Generation-only hosts do not implicitly enable native requests.

The shipped BitRouter skill/setup reference is updated and its entrypoint remains
under 200 lines. Plugin CLI references are unchanged and distribute the same skill;
no CLI command, listen port, env var or default model was added. Product API prose,
English/Chinese synchronization and the docs catalog refresh are a coordinated
`bitrouter-docs` follow-up. Hosted CI and bounded live provider evidence are
recorded above. No deployed Cloud, subscription access or upstream invoice
evidence is implied.

## Parent integration after #964

Merged the updated #962 parent at
`0023b0a50919242cede1580755b002154f2ddeb1` into the Decisions stack. Native
Gemini and retired provider removal remain in force; generation uses Chat
Completions, Messages and Responses, while Decisions retains its own native
codec and endpoint. Shared selected-call transport now retains the parent's
Google replay binding and continuity redaction, together with native completed
failure usage and the no-retry rule. Native calls also reject retired providers
before I/O. Catalog mirrors and the config schema were regenerated.

Validation of this merged source:

- Workspace all-feature nextest: **3696 passed, 22 existing skips**, run
  `44efef5b-6847-4dce-b30d-af28b94ff108`. Nextest marked one existing trajectory
  control test leaky; its isolated rerun passed cleanly, run
  `b01a5c49-077a-408d-bd10-d5de3a1dddfe`.
- The retry-observation fixture preserves the parent's strict tool requirement
  through typed generation input. Its targeted regression passed before the
  complete run, `506ea47c-97f9-496b-8afb-530131640b80`.
- Workspace doctests: **6 passed, 1 ignored**. Strict all-feature Clippy with
  tests, formatting, registry validate/build, schema generation and dist
  freshness passed. AI tests without default features passed.
- Pinned SDK public API listing: native envelopes and observability sentinels
  present; foreign dependency manifest unchanged; no public OTel types.

The A11 live evidence remains scoped to the original tested `aa378f7c` source
and pinned catalog. No live request was repeated during conflict resolution.
Hosted CI for the merged head is separate from the earlier completed CI run.

## Final reviewer privacy fixes

The GPT-6 Astra review of `92bfb8e2` found two confirmed P2 privacy defects.
Both were reproduced with synthetic loopback fixtures and corrected:

- Shared non-streaming SDK execution captures generation continuity values
  before request rendering/authentication. Echoed Anthropic signatures and
  credential-bound Google signatures/replay proofs are filtered from public
  and debug errors; outgoing signatures and source prompts remain unchanged.
- Native full-content telemetry uses a separate JSON projection. It recursively
  removes `safety_identifier` fields and redacts known request-identifier echoes,
  including nested answer extensions. Harmless extensions remain captured, and
  native wire rendering remains unchanged. Capture-off behavior is retained.

The two regressions failed before these production fixes and passed afterward,
run `8bebe244-fe1b-4374-a66a-5453897cc0d6`. The focused Astra follow-up inspected
the fixes and regressions and confirmed both findings closed, with no remaining
actionable defect in that scope.

Final local checks: **3697 all-feature workspace tests passed, 22 existing
skips**, run `085f325c-a4f8-420f-a224-092d300f3cbd`; workspace doctests **6 passed,
1 ignored**; strict all-feature Clippy including tests, formatting and dist
freshness passed. No public API or span-schema shape changed.

Hosted [CI run 37665057763](https://github.com/bitrouter/bitrouter/actions/runs/37665057763)
passed all 22 jobs on the reviewed pre-fix `92bfb8e2` head. Hosted CI for the
privacy-fix head is separate. Prior live-provider evidence remains scoped to
`aa378f7c`; no real credentials or provider calls were used for these fixes.
