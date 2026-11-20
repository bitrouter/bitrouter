# Routing evaluation exchange

Status: **accepted, `eval/1.0.0-beta.1`**.

This contract governs the subjects, results and frozen snapshots exchanged by
BitRouter, external evaluators and offline learners. Clients evaluate actual
outcomes using inference bound to the revision being evaluated. The service
validates and retains evidence. Updating active routing requires an explicit
publication under the [Policy contract](../policy/README.md#management-and-evaluation).

The [public Eval types](../../../crates/bitrouter-ai/src/eval.rs) define the
payloads. The generated schemas cover [subjects](subject.schema.json),
[results](result.schema.json) and [snapshots](snapshot.schema.json).
See the [service API](../service-api/README.md#eval-exchange-api) for
HTTP paths, authorization and errors.

## Version and validation

`EvalSubject`, `EvaluationResult` and `EvalSnapshot` each require
`spec_version: "eval/1.0.0-beta.1"`. Readers MUST reject a missing version,
a version from another namespace or an unsupported revision. Nested decisions,
measurements and snapshot entries use their containing artifact's contract.
Evaluators identify their rubric independently through `evaluator.version`
and `config_digest`.

## Subjects and ownership

A subject describes one evaluation attempt. Its `eval_id` identifies that
attempt and its `subject_id` identifies the logical outcome in the producer's
namespace. Use `request` for a single request, `episode` for a bounded
workflow or conversation, and `task` for an externally defined task with a
terminal outcome. Keep a stable logical identity and assign a different Eval
ID to each attempt.

The required `identity` is a closed object tagged by `profile`.

| Profile | Required fields | Optional fields | Service authority |
| --- | --- | --- | --- |
| `tenant_model_policy` | `installation_id`, `tenant_id`, `configuration_revision`, full `policy` ResourceRef and literal Policy `name` | `source_digest`, `classifier_digest`, `execution` | Tenant Eval |
| `tenant_generic` | `installation_id`, `tenant_id` | `execution` | Tenant Eval |
| `gateway_generic` | The profile tag alone | — | Gateway Eval |

For a model-policy subject, validate the complete tuple against the actual
retained configuration and exact Policy binding, including its resource owner
and revision. `auto` and `auto:cost` are distinct names. Diagnostic digests
supplement that identity. An optional execution link names the exact
`execution_id` and `configuration_revision` of an owned receipt. Correlation
and subject IDs convey no execution authority. Reject mixed fields, an identity
outside the addressed profile, and mismatched references before storing it.

Results and snapshots inherit verified subject identity. Model-policy exports
and aggregation preserve the whole tuple, including immutable revision and
literal name. Only the model-policy profile can supply evidence attributed to
that Policy revision.

## Decision measurements and evidence

`route_projection` and `request_key` each encode the [Policy
`RouteClass`](../policy/README.md#model-selection) as a case-sensitive
`task-family|next-step|risk` string. Supply the exact classifications and
decision ID emitted by the router. Keep selected and baseline tiers and effort,
and the diagnostic Policy digest, as reported. Each decision's Policy name
matches the subject's literal Policy name when the subject has a model-policy
identity.

An optional `route_measurement` describes the reported model action distribution.
Each candidate identifies its tier, model, optional reasoning effort and logging
probability in parts per million. Provide 1–64 unique candidates in canonical
(tier, model, effort label) order, using the effort label `inherit` when absent.
Probabilities sum to 1,000,000 ppm. The logged action must match one candidate
and its positive probability. `candidate_set_digest` binds the ordered target
identities. `selected_tier` records the effective choice after model-selection
guards. When exact attribution is unavailable, omit the decision from the
subject.

Evidence entries have a stable ID, namespaced kind, `sha256:` digest with 64
lowercase hexadecimal digits, `redacted: true` and optional safe string
attributes. The subject's `evidence_digest` seals the canonical evidence list.
Keep credentials and raw request, tool and verifier data private. Use
`requested_dimensions` to bound the metrics an evaluator may submit.

## Results

Repeat the subject's `eval_id` and sealed `evidence_digest`. Identify the
evaluator and its fixed rubric, and cite subject evidence IDs in
`evidence_refs`. Keep the idempotency key and immutable payload stable on an
equivalent retry. A result requires a verdict of `pass`, `fail` or
`inconclusive`. Omitted metrics express absence. A value of zero is an
observation.

| Unit | Integer value |
| --- | --- |
| `boolean` | 0 or 1 |
| `ppm` | 0–1,000,000 |
| `micro_usd` | Nonnegative USD micro-units |
| `milliseconds` | Nonnegative milliseconds |
| `count` | Nonnegative count |
| `scalar_micros` | Signed scalar micro-units |

`confidence_ppm` expresses confidence in the verdict and may be null.
Decision credit names a subject decision. Weights range from 0 to 1,000,000
ppm and apply to the submitted metrics, declared violations or `quality.pass`
named in `metric_ids`. An empty metric list applies to all present metrics.
An empty credit map assigns full credit for a single decision and withholds
attribution for multiple decisions. An explicit zero weight withholds credit.

Evidence admission distinguishes `admitted`, `held_out`, `rejected` and
`disputed`. Preserve the reported disposition. A submission receipt by itself
does not confer update eligibility.

## Frozen snapshots

`EvalSnapshot` records a frozen membership as `evidence_root`, `frozen_at`
and ordered `entries`. Each entry binds `result_id`, `eval_id`, the immutable
result `content_digest` and its `subject_content_digest`. Subject identity
and decisions are covered by the subject digest. Entries are ordered by result
ID. Later evaluations or changes to admission do not change the frozen set.
`frozen_at` is an RFC 3339 label. When supplied, preserve its exact decoded
string for the reply and root commitment. The snapshot carries no Policy
activation authority.

## Conformance

Reject missing, wrong-namespace and unsupported `spec_version` on each of the
three artifacts. Reject invalid route classes and mixed or mismatched subject
identities. Verify exact authorized replay, immutable snapshot membership,
cross-tenant isolation and separation of same-name Policy revisions. Submitting
or freezing evidence leaves the active Policy and configuration intact.
