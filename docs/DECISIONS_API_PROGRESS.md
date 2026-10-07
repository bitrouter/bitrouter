# Decisions API implementation progress

Status: **implementation in progress; full gateway acceptance pending.**

The contract is [DECISIONS_API_SPEC.md](DECISIONS_API_SPEC.md). The implementation
branch is `codex/decisions-api`, stacked on #962 at `529f2fde`; the approved spec
is preserved in `3676c99a`. No credentialed OpenAI call, push, publication or deployment has run.

## D1: native AI semantics and selected-target client

Implemented locally on 2026-10-06:

- Native text/image evidence, three question/answer families, typed boolean/string
  choices, refusals and explicit nullable fields.
- Separate native JSON codec and transport, without generation/SSE adapters.
- Positional answer/name/type/distribution validation, independent usage decoding
  and safe completed-response failures retaining available accounting evidence.
- Same-wire additive fields with a combined 64 KiB extension budget. Probability
  totals and weighted score comparisons use absolute tolerance `0.0001`; native
  values remain unchanged.
- `ModelClient::decide` shares selected-account authentication, bounded 401
  recovery, HTTP timeouts/cancellation and credential diagnostic filtering.
- `ApiProtocol::Decisions` and operation mismatch guards. Generation conversion
  admission rejects Decisions. Existing generation fixture helpers use fallible
  lookup and keep the four-by-four conversion matrix.

Local evidence:

- New Decisions integration tests: **9 passed** (codec, image bounds, native
  nulls, refusals/failure usage, invocation, mismatch/cancellation and auth).
- `cargo clippy -p bitrouter-ai --all-features --tests -- -D warnings`: passed.
- `cargo nextest run -p bitrouter-ai --all-features --test-threads 2`: **585
  passed, zero skipped**, run `ec166c39-0911-40f6-a457-ef1b6ebff0d8`.
- AI all-feature doctests: **1 passed, 1 ignored**.

These prove local AI/mock behavior only. They do not prove SDK/application
migration, gateway settlement, real OpenAI behavior, full-workspace checks or
hosted CI. D1 is a foundation within the complete stacked change.

## D2: SDK and application migration

Implemented locally in the working tree on 2026-10-06:

- `PipelineInput` and `PipelineOutput` carry their native operations. Generation
  input uses `Box<Prompt>` to satisfy Clippy's enum-size check; the existing
  constructor and borrowing accessors retain typed `Prompt` semantics.
- Context consumers use explicit generation/decision access. Generation defaults
  and effort mutations are fallible. Response construction requires a completed
  execution and does not synthesize an empty generation result.
- Native Decisions preflight and execution share selected HTTP authentication,
  fresh body rendering after bounded 401 recovery, provider/trace/request-ID
  headers, and the existing detached execution/delivery/shutdown machinery.
- Hard operation filtering precedes preferred-protocol selection, applies to
  provider pins and mixed lists, and is rechecked after route hooks. Completed
  invalid Decisions output retains usable usage and bypasses custom retry policy.
- Hook registrations declare supported operations; host requirements independently
  name the concrete protection and stage. Existing hosts default to generation.
  App assembly explicitly enables both operations and requires shared auth,
  session identity, reserved judge protection, policy and metering. Generative
  continuation, tools, selectors and predictive observers remain scoped.
- Bound router defaults, policies and native checker compatibility are checked
  before incompatible preparation or external checker work. Native fragments
  cover evidence, question names/instructions, typed choices and rubric text;
  images are counted as excluded coverage and safety IDs remain excluded.
- `/v1/decisions` uses the shared gateway handler with native JSON codecs and the
  normal correlation header. Streaming and unsupported fields fail admission.
- Telemetry consumes typed usage/operation. Span schema version 2 declares native
  root/hop names and protocol metadata; optional native content capture excludes
  the safety identifier. The committed span artifact has been regenerated.

Local evidence:

- Native SDK integration tests: **9 passed**, covering gateway fidelity, model
  projection, scoped/required hooks, mixed protocol lists/pins, checker preflight
  and coverage, pre-completion fallback, completed invalid-output settlement,
  and observed client disconnect plus shutdown drain.
- `cargo clippy -p bitrouter-sdk --all-features --test decisions -- -D warnings`:
  passed after boxing the generation payload. Strict checks including migrated
  legacy fixtures remain part of final validation.
- `cargo nextest run -p bitrouter-sdk --all-features --test-threads 2`: **720
  passed, 2 pre-existing skips**, run `6d7a8d23-3c92-4630-b9d7-93006c378c39`.
- Span artifact regeneration/check: passed. Generation assertions have been
  migrated to typed extraction and retained in the passing SDK suite.
- All-feature workspace test compilation and strict Clippy passed:
  `cargo clippy --workspace --all-features --tests -- -D warnings`.
- Full workspace all-feature nextest: **3,750 passed, 22 existing skips**, run
  `de6442e3-278a-4a35-9658-cfd492650b72`. This includes application continuation,
  evolution, policy, metering and gateway regressions, plus telemetry/guardrails.
- Full workspace all-feature doctests: **6 passed, 1 ignored**.
- `cargo fmt -- --check` and `git diff --check`: passed.

These checks prove the current local D2 implementation and generation regression
baseline. Application-specific Decisions authorization/budget/reserved-ID cases,
full pricing/registry acceptance, external consumers, hosted CI and live OpenAI
verification remain part of the complete feature audit.

## D3: protocol tariff data and lookup foundation

Working-tree progress after D2 commit `9e966830`:

- Runtime and AI catalog models parse independent `pricing_by_protocol` maps.
  Exact overrides win; generation may use ordinary pricing when its override is
  absent; Decisions cannot use that fallback. An empty explicit override stays
  present and its missing buckets do not inherit ordinary rates.
- Registry source supports the Decisions token and independent tariffs. Dist
  generation translates tariff keys into runtime names. Source append/sync
  serialization preserves protocol maps and context tiers.
- Application registry mapping and pricing-table assembly retain canonical/native
  aliases for each override. Assembly reads its effective routing configuration.
- SDK stream usage pricing, metering, evaluation and route previews select the
  actual outbound wire. Native nonzero-cache usage is provisionally unavailable
  until the cache-billing interpretation is verified. Missing target/protocol
  evidence is unavailable rather than reconstructed from the model name.
- Restart-required pricing signatures include protocol overrides.

Current local evidence:

- Strict all-feature workspace Clippy, including tests: passed for these changes.
- Four targeted tariff tests passed (independent/missing buckets, no native
  generation fallback, 272,000/272,001 threshold, registry mapping/key translation
  and append preservation), run `b386fb37-9ca5-4749-9db4-1d4556219fe2`.
- Wider pricing, metering, evaluation and registry subset: **189 passed**,
  run `985061df-8f42-4b95-83e1-b8bc3e396ab2`. Standalone metering fixtures now
  provide explicit serving-target/protocol evidence instead of implying it from
  model names. This is a targeted subset, not a new full-workspace acceptance run.

### Frozen tariff implementation

Implemented locally after the foundation checks:

- Read-only `RouteHook::after_resolve` captures each effective target after every
  mutable route hook and operation/effort filter. App assembly independently
  requires `CaptureTariffs` for both operations and composes capture from the
  same metering recorder's assembly-time table.
- `TargetTariffSnapshot` retains protocol, effective endpoint profile, complete
  rates/tiers, version and the native zero-cache billing condition. Effective
  cross-profile endpoint overrides make cost unavailable; custom URLs are
  represented by digests.
- Metering and evaluation call one settlement calculation over the matching
  snapshot. SDK streaming consumes its `UsagePricingSnapshot` projection;
  explicit frozen unknown prices disable live table lookup.
- Opt-in `server.require_known_pricing` rejects incomplete coverage before I/O,
  including native cache uncertainty. Its default is false. Tariffs, configured
  endpoint profiles and this requirement are restart-required.
- Charge evidence uses an optional frozen snapshot for backward-compatible
  deserialization. Exports/overrides retain it and cannot bypass the native cache
  gate. Authoritative correction retains the original admission evidence;
  corrupt stored evidence fails reconciliation.
- Custom executor native results are validated before success hooks; malformed
  typed success retains usable usage and cannot retry through custom fallback.
- Shipped skill/setup reference, development guide and explicit alpha migration
  inventory were updated. The skill entrypoint remains under 200 lines.

Current validation:

- All-feature workspace selected regressions: **119 passed**, run
  `fe5c5b3e-f729-4694-bc5e-0c5de261654a`. This includes actual HTTP upstream /
  native gateway / SQLite accounting for valid, malformed, refusal and cached
  outputs; final-route capture; shared eval evidence; SDK stream frozen/unknown
  prices; cache-gated exports; thresholds/profiles; reload policy and legacy
  metering. This is a targeted subset, not full-workspace final acceptance.
- Strict all-feature workspace Clippy including tests: passed after the receipt
  preservation assertion was added (`frozen-tariffs-clippy4`, 47.01s).
- The earlier build exhausted disk before executing tests. Only this checkout's
  generated Cargo artifacts were cleaned (60.2 GiB); subsequent validation uses
  `CARGO_INCREMENTAL=0`, dev/test debug 0 and two build jobs. The 119-test run
  passed with those resource settings. No source/worktree state was removed.
- The expanded native gateway/cache/export/authoritative-receipt fixture passed
  separately after its final assertion, run
  `d9aaa354-e4b7-4150-9d38-866085a7148d`. The real SQLite correction retains the
  original native tariff snapshot and records only that synthetic receipt's
  request amount; it does not establish upstream billing semantics.
- `cargo fmt -- --check`, whitespace checks, Markdown fences and local links:
  passed.

D3/D4 acceptance remains open: catalog-derived tariffs need declared endpoint
profile provenance so configured regional endpoints cannot silently inherit
published global native rates. Registry activation and generated schema/catalog
artifacts must include that contract. Application-specific native guards,
full-workspace checks, public API/feature isolation, final lifecycle/privacy
coverage and provider proof remain.

Read-only external inventory is in [DECISIONS_API_MIGRATION.md](DECISIONS_API_MIGRATION.md).
The Cloud checkout at `184c1f2e` still resolves published alpha.30 and has not adopted
the parent SDK extraction. No Cloud files, build or deployment were changed;
its untracked `lib/` was preserved. PR #962 remains open at `529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16`.

On 2026-10-06, `OPENAI_API_KEY` and the default credential store's OpenAI API-key
slot were absent. No credentialed call was made; A11 remains unverified. The
upstream Decisions/Luna pricing pages were refreshed and still do not resolve
whether cached subsets are included in the base input charge. The accepted
cache-billing gate remains in place.

## Remaining work

| Batch | Current state |
| --- | --- |
| D2 | Lifecycle implemented; full workspace regression baseline green; Decisions-specific app guard coverage and final audit remain |
| D3 | Protocol tariffs and frozen settlement implemented locally; selected regressions pass; declared catalog profile provenance and full acceptance remain |
| D4 | Skill/setup and external inventory updated; registry/profile/schema/API artifacts, full-workspace checks and qualified provider proof remain |

Spec acceptance A1-A12 remains pending as an end-to-end audit. Local AI/SDK
evidence must not be presented as completed gateway support. Nonzero-cache
Decisions pricing remains an explicitly unresolved upstream billing gate.
