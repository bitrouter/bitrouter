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

## Remaining work

| Batch | Current state |
| --- | --- |
| D2 | Lifecycle implemented; full workspace regression baseline green; Decisions-specific app guard coverage and final audit remain |
| D3 | Pending: protocol tariffs, frozen charge evidence, cache-billing gate and reporting; native lifecycle settlement has local SDK evidence |
| D4 | Pending: registry/artifact/skill updates, full-workspace checks, provider verification and external-consumer inventory |

Spec acceptance A1-A12 remains pending as an end-to-end audit. Local AI/SDK
evidence must not be presented as completed gateway support. Nonzero-cache
Decisions pricing remains an explicitly unresolved upstream billing gate.
