# Classifier API implementation and acceptance

Date: 2026-10-08. Branch: `codex/classifier-api`. Starting source:
`d6dfc245be38c7e6d3977b0528cbcdf694fac246`.

Status: implemented and verified locally; credentialed provider conformance
remains pending. All 3,730 executed workspace tests pass; 25 opt-in tests are
skipped. Doctests, Clippy, formatting and generated artifact checks pass. The governing contract is
[CLASSIFIER_API_SPEC.md](CLASSIFIER_API_SPEC.md); public source and persisted
evidence migration is recorded in [CLASSIFIER_API_MIGRATION.md](CLASSIFIER_API_MIGRATION.md).

## Implemented behavior

AI owns canonical `ClassifierRequest`/`ClassifierResult`, the non-streaming
classifier codec interface, Decisions/System One native projection and
`ModelClient::classify`. SDK `model_call` owns the shared routing, conversion
admission, protection, delivery and settlement lifecycle. Both native HTTP
endpoints are mounted under their original protocol names.

Native System One preserves structured state/instructions/rubrics, optional and
nullable instructions/criteria, map keys, structured Score legends, partial
usage and admitted additive metadata. Native Decisions preserves text/image
evidence, typed choices, optional/duplicate names, refusals and raw usage.
Canonical serialization preserves structured arrays and original wire identity.
Duplicate JSON members cannot silently erase questions, answers or usage.

System One callers can use the admitted OpenAI subset: string evidence and
instructions, Predicate without separate criteria, string Choice and simple
Score rubrics. Choice/Score output uses TypeSafe's distribution statistic while
canonical evidence retains the reported upstream confidence. Rich requests
exclude incompatible targets before dispatch. All supported classifier
protocols for a provider remain candidates; its preferred wire does not hide a
compatible alternative. Model discovery exposes active operations/protocols
within the host's served scope.

Decisions callers cannot route to System One while its missing OpenAI-required
usage breakdown remains unresolved. This is an admission gate, including for
otherwise compatible predicates. Codec conversion fixtures do not advertise
this direction as gateway support.

Completed refusals that cannot fit System One, malformed answers, acknowledged
HTTP 200 responses whose body cannot be read, and invalid
custom executor results fail delivery without fallback or replay. Independently
decoded provider usage survives where available. Settlement records failure
once, after caller-response compatibility is assessed. Disconnect and graceful
shutdown retain admitted execution and settlement work.

System One pricing freezes the actual outbound wire and endpoint. It records
reported billable input units independently of unknown cache/reasoning buckets,
keeps free output counters and rejects unverified rate conditions. Decisions'
cache-billing uncertainty remains intact. SQLite stores partial usage and the
frozen tariff through the existing evidence columns.

## Evidence matrix

| Contract | Evidence | Current boundary |
| --- | --- | --- |
| Native codecs, canonical serialization and duplicate identity | `crates/bitrouter-ai/tests/decisions.rs`, `systemone.rs` | Controlled fixtures; no real provider conformance |
| Conversion, correlation and destination confidence | AI mixed-primitive and confidence fixtures; SDK HTTP conversion case | Usage-gated direction remains excluded |
| Native gateway, admission and alternate protocol | `crates/bitrouter-sdk/tests/decisions.rs` | Controlled HTTP upstreams |
| Refusal/output errors and once-only settlement | SDK refusal/malformed/custom-executor cases | No upstream invoice proof |
| Usage and actual tariff settlement | `same_systemone_caller_uses_actual_outbound_tariff_in_sqlite` and native accounting cases | The same caller endpoint returns native usage and charges 4/10 micro-USD from the actual System One/Decisions fixture tariffs; generation rates cannot leak |
| Identity, checks, scopes, cancellation and shutdown | SDK lifecycle and classifier request-check cases | Local process evidence |
| Telemetry availability and semantic/native naming | Telemetry success/failure fixtures and regenerated SDK span schema v3 | Root usage retains unknown flags; native protocol strings remain unchanged |
| Legacy semantic aliases and native wire strings | AI serialization tests and SDK scope/fragment fixtures | External export consumers need coordinated rollout |
| Partial-usage consumers | Requests JSON/human views; workflow archive validation and mixed summaries | Availability and provenance survive; unavailable totals differ from reported zero; input-only evidence rejects invented buckets |
| Registry and configuration schema | dist-helper validate/build/check | Passed; generated source catalog and schema are current |
| Generation regressions and workspace migration | Full nextest: 3,730 passed, 25 skipped; doctests: 6 passed, 1 ignored; Clippy and formatting passed | Ignored live cases remain unrun; no hosted CI claim |
| Credentialed native and converted gateway | Opt-in `classifier_live_tests.rs` | Not run; credentials absent from this process |
| External Cloud and production | [Migration inventory](CLASSIFIER_API_MIGRATION.md) | Read-only inventory only; no Cloud build/deploy |

The GPT-6 Astra specification review identified a destination-confidence gap.
Section 5.2 of v0.2 defines the rendering rule; fixtures deliberately use reported
confidence different from TypeSafe's statistic. The review found no additional
actionable architectural defect. This review does not substitute for tests or
credentialed conformance.

## Credentialed validation

Three ignored app tests exercise native System One, native Decisions, and a
System One caller served by Decisions. Each sends Predicate/Choice/Score through
the actual selected HTTP executor, validates caller correlation and usage, and
checks exactly one SQLite settlement with the actual outbound frozen tariff.
They fail explicitly if their local key file is unavailable.

Run one case with an explicit key-file path, for example:

```sh
TYPESAFE_API_KEY_FILE=/absolute/path/to/local-key \
  cargo test -p bitrouter --all-features \
  live_systemone_native_gateway_and_settlement -- --ignored
```

The OpenAI cases use `OPENAI_API_KEY_FILE`. Optional
`TYPESAFE_CLASSIFIER_MODEL`/`OPENAI_CLASSIFIER_MODEL` select an explicitly tested
model; defaults are `jev-1.13.0`/`gpt-6-luna`. Recheck the independent catalog
tariff when changing a model. Set `BITROUTER_CLASSIFIER_SMOKE_EVIDENCE_DIR` to
save redacted JSON with actual reported model, native wires, response usage and
SQLite settlement. Keys are not printed or included in that evidence.
These tests make provider calls; successful runs establish bounded local
conformance, not invoice billing, hosted CI or production deployment.

## Local check execution

The initial full-test compilation exceeded local disk capacity. Only this
checkout's stopped-build artifacts were removed. The retry uses
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`; these affect build artifacts, not feature or test
selection. The final full nextest run passed all 3,730 executed tests with 25 skipped,
including credentialed/opt-in cases. Registry validation completed with existing
warnings about unrelated non-curated models and floating runtime tags; registry
build and dist/schema freshness checks passed. No warnings were bypassed.

Final verification also passed the two telemetry conformance cases after the
Clippy-only expression cleanup. Clippy reports no project lint warnings; its
remaining future-incompatibility notice names the existing third-party
`proc-macro-error2` dependency. The macOS linker reported its existing unwind
section size warning while linking the app test binary. Both are recorded
without suppression. `git diff --check` passes.

## Requirement audit follow-up

The completion audit added direct proof for previously indirect claims:

- `equivalent_native_requests_share_the_three_primitive_semantics` compares
  Predicate, Choice and Score after both native ingress paths and both request
  projections, retaining correlation metadata separately.
- `same_systemone_caller_uses_actual_outbound_tariff_in_sqlite` uses one caller
  request and endpoint against both upstream wires, asserts exact native bodies,
  original response keys, raw provider usage, once-only settlement and each
  actual frozen tariff.
- `classifier_input_only_evidence_validates_without_inventing_buckets` checks
  archive decoding, summaries, native totals and input-only charges; missing
  masks, altered totals, invented buckets and a changed native wire fail.
- `unknown_usage_is_distinct_from_reported_zero_in_both_views` verifies requests
  JSON provenance and human rendering. Unknown totals show `?`; estimates show
  `~`; reported zero remains `0`.
- Native response rendering rejects structured Score labels on Decisions,
  while System One retains its structured legends. Conversion reports mention
  confidence derivation only for Choice/Score requests.
- `successful_classifier_headers_with_broken_body_never_replay_work` first
  reproduced a second call under a retry-all policy after HTTP 200. The executor
  now records the acknowledged classifier response as non-retryable before
  propagating its body-read error. The regression proves one failed settlement
  and no fallback call; ordinary pre-success HTTP failures retain their policy.

These are local deterministic proofs. Credentialed TypeSafe/OpenAI native and
converted calls remain unrun until the required key-file paths are supplied.

The final audit suite passes all 3,730 executed tests, with 25 opt-in tests
skipped. The full run flagged an output-pipe leak for the existing ACP
`unpinned_codex_acp_never_receives_cli_config_arguments` case; its isolated rerun passed without a leak flag. Doctests, Clippy, formatting and dist freshness
checks passed against the audit changes.
