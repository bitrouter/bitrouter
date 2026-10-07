# Decisions API implementation progress

Status: **implementation in progress; full gateway acceptance pending.**

The contract is [DECISIONS_API_SPEC.md](DECISIONS_API_SPEC.md). The implementation
branch is `codex/decisions-api`, stacked on #962 at `529f2fde`; the approved spec
is preserved in `3676c99a`. No provider call, publication or deployment has run.

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

## Remaining work

| Batch | Current state |
| --- | --- |
| D2 | In progress: typed SDK envelopes, scoped/required hooks, checker projection, routing and HTTP endpoint |
| D3 | Pending: protocol tariffs, frozen price evidence, failure/disconnect settlement and reporting |
| D4 | Pending: registry/artifact/skill updates, full-workspace checks, provider verification and external-consumer inventory |

Spec acceptance A1-A12 remains pending as an end-to-end audit. Partial D1
evidence must not be presented as completed gateway support. Nonzero-cache
Decisions pricing remains an explicitly unresolved upstream billing gate.
