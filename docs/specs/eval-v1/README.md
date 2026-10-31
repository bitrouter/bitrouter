# Eval exchange HTTP profile

Status: **beta, specification revision 1.0.0-beta.1**. This profile records the
legacy `/v1/evals` exchange at `80550c9b`; tenant-scoped Eval uses the separate
[tenancy profile](../tenancy-v1/README.md#tenant-eval-profile). Beta revisions
may change incompatibly. MUST, MUST NOT, SHOULD and MAY express requirements.

The optional `core` feature exposes the evaluator-neutral evidence exchange.
These endpoints append and read evidence; they do not publish a policy lock or
start a model invocation. Authentication follows the configured gateway
credential policy, including explicit development `skip_auth` configuration;
production clients MUST use the host's configured authentication boundary.

| Method | Path | Body / response |
| --- | --- | --- |
| POST, GET | `/v1/evals/subjects` | `EvalSubject` / insertion report or subject list |
| GET | `/v1/evals/subjects/{eval_id}` | Subject |
| POST | `/v1/evals/results` | `EvaluationResult` / admission outcome |
| POST | `/v1/evals/snapshots` | Optional `FreezeRequest` / frozen snapshot |
| GET | `/v1/evals/snapshots/{evidence_root}` | Frozen snapshot |
| GET | `/v1/evals/status` | Ledger status |

Fields, identity, numeric units, result and error envelopes are defined by the
[public Eval types](../../../crates/bitrouter-ai/src/eval.rs). Results refer to
subjects; evidence snapshots represent frozen membership, not live policy.
Subject and result identity are immutable: matching replay is distinguished from
conflicting data. Observation of transport success or cost does not by itself
establish task quality. Clients MUST handle authentication, invalid input,
missing subjects and server failures as failures, and MUST NOT use a network
failure as proof that a prior write did not commit. Unknown optional response
fields may be ignored; unknown required semantics MUST NOT be guessed. See
[conformance](conformance.md) and the [operator workflow](../../../skills/evaluating-bitrouter-routes/references/eval-exchange.md).
