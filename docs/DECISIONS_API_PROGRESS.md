# Decisions API implementation progress

Status: **implemented; local/hosted checks and bounded live OpenAI acceptance verified.**

The contract is [DECISIONS_API_SPEC.md](DECISIONS_API_SPEC.md). The implementation
branch is `codex/decisions-api`, stacked on #962 at `529f2fde`; the approved spec
is preserved in `3676c99a`. Draft stack: [#965](https://github.com/bitrouter/bitrouter/pull/965), based on #962.
Two bounded credentialed OpenAI calls are verified below; no deployment has run.

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

## D4: catalog activation, profile provenance and final audit

Implemented locally on 2026-10-06/07:

- Pricing in source/dist/AI catalog, SDK config and application snapshots retains
  declared `endpoint_profile` provenance. Global/US/European declarations are
  typed metadata; configured regional hosts cannot rebind inherited global
  rates. Unknown/custom endpoints receive no inferred premium. Profile changes
  remain restart-required and context resolution preserves the declaration.
- SDK-only live stream price lookup also rejects a declared profile mismatch.
  Registry append/serialization keeps profile metadata and tier inheritance.
- Only OpenAI's API-key Luna model advertises native Decisions; existing
  generative order and ordinary prices remain. Native global $0.10/$0.20 rates
  are tagged `openai_global`, with the existing cache-billing gate retained.
- Main and embedded catalog artifacts and the config schema are regenerated.
  Registry validation/build and the dist freshness check passed.
- Real native app gateway tests enforce virtual-key authentication, key/policy
  expiry, ACLs, spend and rate limits. The shared reserved-ID guard rejects
  native replay and preserves the metering namespace. Native telemetry conforms
  to the committed span schema; capture is off by default and safety ID is
  excluded from full content capture.
- A real TCP gateway cancellation test observes handler cancellation after a
  closed socket, verifies shutdown waits for admitted HTTP work, and confirms
  exactly one SQLite settlement. Direct AI native cancellation/read-timeout
  tests observe upstream admission and confirm one request without replay.
- Response extension bounds include nested usage additions. Completed malformed
  custom output retains only independently decoded provider counters; oversized
  usage additions fail delivery and preserve required raw counters within the
  bound. Wrong-operation generation usage cannot become native charge evidence.
- Changed fixture panic calls were removed while retaining assertions. The ACP
  control-socket spend fixture now supplies its serving target and admitted
  tariff; its existing 70-micro-USD assertion is unchanged.

Local verification:

- Full workspace all-feature nextest: **3769 passed, 22 existing skips** after
  the final in-flight client fixture and panic-call audit, run
  `258bdf17-8ab9-4c7d-ac79-8f5a0d7837b1`. The preceding complete
  run passed 3768 tests, run `9c203624-cf63-4b4a-afb9-d547e19f5a1c`.
- Workspace doctests: **6 passed, 1 ignored**. Strict all-feature Clippy with
  tests, formatting and dist freshness passed for the preceding complete run.
  The final score-rubric projection assertion passed separately, run
  `6796daa9-5977-46c2-9dfb-fd34b6fae5f1`; strict Clippy, formatting and dist
  freshness passed again after that final fixture change.
- Native/pricing/guard/telemetry selected subset: **137 passed**, run
  `ce40dcc1-41bc-43cd-957d-bf9b0bab8100`.
- Pinned SDK public API listing (`nightly-2026-05-05`, tool 0.52.0), positive
  sentinels, unchanged foreign-dependency manifest and OTel exclusion: passed.
- CI feature matrix (AI minimal/pkce/hosted-login/file-store; SDK
  minimal/config/server/ACP; guardrails minimal/SDK; telemetry HTTP/gRPC/server)
  and positive-backed dependency guards: passed.
- No new lint bypasses, public forwarding exports or panic calls remain in the
  Rust diff against the parent. Old fixture assertions are retained.

Requirement-level evidence and qualifications are in
[DECISIONS_API_ACCEPTANCE.md](DECISIONS_API_ACCEPTANCE.md). Final rubric validation is complete;
source-state review is complete and draft #965 is published. Support claims stay scoped to
these local checks and the retained upstream billing gate.

Read-only external inventory is in [DECISIONS_API_MIGRATION.md](DECISIONS_API_MIGRATION.md).
The Cloud checkout at `184c1f2e` still resolves published alpha.30 and has not adopted
the parent SDK extraction. No Cloud files, build or deployment were changed;
its untracked `lib/` was preserved. At initial delivery, PR #962 was open at
`529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16`. By the live acceptance run it had
advanced to `0023b0a50919242cede1580755b002154f2ddeb1`; this live evidence is
scoped to the existing `aa378f7c` stack, without a parent rebase in this test.

Before the user supplied a temporary key on 2026-10-07, `OPENAI_API_KEY`, the
default credential store's OpenAI API-key slot and the configured OpenAI
provider/account keys were absent. A11 was initially unverified as permitted by
the spec. The beta HTTP and typed resource references and pricing pages were refreshed. Cached-subset billing
remains unresolved; documented multipliers are estimates rather than invoice proof.

## Delivery status

All four batches are implemented and locally verified, committed and published
as draft #965 on #962. Hosted CI passed all 22 jobs at `aa378f7c`. A11 passed
two bounded API-key gateway calls on 2026-10-07; deployed Cloud support remains
unverified.
The external SDK migration and product-docs/catalog follow-up are inventoried;
neither is advertised as a completed deployment. This local ledger does not
establish upstream cache billing or invoice amounts.

## A11: bounded live OpenAI acceptance

At 2026-10-07 17:33 UTC, an isolated gateway built from `aa378f7c` invoked the
real OpenAI `/v1/decisions` endpoint with the user-supplied temporary key. Text
predicate/boolean-choice/score and inline-image predicate calls both returned
HTTP 200. Usage was 426 and 165 input tokens, zero output/cache tokens.
Graceful shutdown completed and exactly two SQLite rows retained matching raw
usage, native protocol and frozen global tariffs. Total configured estimate:
60 micro-USD; no invoice evidence.

The default registry fetched `main`, whose Luna entry lacked this unmerged PR's
native metadata and rejected admission locally. The successful run selected the
immutable PR catalog through `registry.url`; no source fix or public deployment
was needed. See [acceptance scope](DECISIONS_API_ACCEPTANCE.md) and
[redacted live evidence](DECISIONS_API_LIVE_EVIDENCE.json). The gateway is stopped
and the key was not saved in configuration, credentials or committed evidence.

## Parent conflict resolution

The stack now incorporates #962 at `0023b0a5`, including #964's Gemini
retirement. Resolution preserves the three generation protocols, native
Decisions, Google credential-bound replay/redaction and operation-specific
usage settlement. Catalog/schema artifacts are current. Full all-feature
nextest passed 3696 tests with 22 existing skips; the one leaky control fixture
passed cleanly in isolation. Doctests, strict Clippy, formatting, minimal AI
tests and pinned public API guards passed. See the acceptance ledger for run
IDs and the separate scope of prior live/hosted evidence.
