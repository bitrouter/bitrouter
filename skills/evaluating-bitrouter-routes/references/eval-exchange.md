# BitRouter Eval Exchange Reference

Use this reference to build evaluator-owned packets for the current BitRouter
Eval Exchange. It is a wire contract, not a policy-authoring interface.

## Subject and sealing contract

An `EvalSubject` has these required fields:

| Field | Meaning |
|---|---|
| `schema_version` | Integer `1`. |
| `eval_id`, `subject_id` | Non-empty bounded identifiers. `eval_id` identifies the immutable evaluation attempt; `subject_id` identifies the logical request, episode, or task within its caller-defined namespace. |
| `scope` | `request`, `episode`, or `task`. Select the smallest scope containing the outcome. |
| `policy_digest` | The exact `sha256:<64 lowercase hex>` policy digest observed by the router. |
| `preset`, `cohort` | Optional evaluator metadata. Do not use them to reconstruct policy identity. |
| `holdout` | Boolean. A held-out subject cannot become training evidence. |
| `decisions` | Zero or more router-authored decision references. |
| `requested_dimensions` | Namespaced metric ids the evaluator may submit. |
| `evidence` | Redacted evidence items only. |
| `evidence_digest` | Canonical digest of `evidence`; leave this empty in a draft and let `subject seal` populate it. |
| `observed_at` | RFC 3339 timestamp. |

Choose `scope` from the evidence packet's observable boundary:

| Observable boundary | Scope |
|---|---|
| The verdict concerns one request-local outcome. | `request` |
| The verdict concerns a bounded workflow or conversation spanning multiple requests. | `episode` |
| The verdict concerns an externally defined task with a task identity and terminal task or verifier outcome. | `task` |

Decision count and evaluator organization do not determine scope. For example,
a bounded multi-request enterprise workflow is an `episode` unless its packet
names an externally defined task and terminal task outcome.

For repeated evaluations of one task, keep each attempt's `eval_id`, result,
and evidence distinct while keeping `subject_id` stable inside the same
run/source/policy namespace. The route compiler uses `subject_id` to deduplicate
independent tasks; attempt-specific subject ids would incorrectly turn retries
into independent evidence. Include enough namespace in the subject identity to
avoid collision with an unrelated run or policy population.

Each `decisions[]` entry is exactly:

```json
{
  "decision_id": "router-decision-id",
  "policy": "auto",
  "route_projection": "agent_route/v1|code:generation|implement|normal",
  "request_key": "agent_route/v1|unknown|implement|normal",
  "selected_tier": "economy",
  "baseline_tier": "strong",
  "policy_digest": "sha256:...",
  "experiment": {
    "experiment_id": "sha256:...",
    "arm": "challenger",
    "assignment_unit": "task",
    "assignment_id_digest": "sha256:...",
    "challenger_propensity_ppm": 100000
  }
}
```

Copy every decision field from router evidence. `selected_tier` is the actual
route; `baseline_tier` is the router's comparator. Never infer either from a
preset or rewrite a known baseline. `experiment` is optional for backward
compatibility. When present, it is router-authored signed assignment evidence:
copy the whole object verbatim. Evaluators must never invent or edit an
experiment id, arm, assignment unit, assignment-id digest, or challenger
propensity. The evaluator-owned top-level `cohort` string is metadata and never
determines optimizer cohort membership.

Each `evidence[]` entry contains `evidence_id`, namespaced lowercase `kind`,
`digest` (`sha256:<64 lowercase hex>`), `redacted: true`, and optional safe
string `attributes`. Attributes must not contain credential-shaped material.
Keep raw private materials outside this object.

Seal a JSON or YAML draft locally:

```bash
bro eval subject seal subject-draft.yaml --output subject.json
```

The command sorts evidence by `evidence_id`, calculates BitRouter's canonical
digest, validates the resulting subject, and writes deterministic pretty JSON.
It does not open or mutate the evidence ledger. If validation fails, correct the
draft and seal a new file; do not invent a digest.

## Result contract and units

An `EvaluationResult` requires:

```text
schema_version: 1
eval_id: exact sealed subject id
evidence_digest: exact sealed subject digest
evaluator: { authority_id, evaluator_id, kind, version, config_digest }
verdict: pass | fail | inconclusive
metrics: { metric_id: { value, unit } }
hard_violations: [metric_id]
confidence_ppm: optional integer 0..1000000
evidence_refs: [subject evidence_id]
decision_credit: { decision_id: { weight_ppm, metric_ids } }
idempotency_key: stable non-empty identifier
submitted_at: RFC 3339 timestamp
```

Copy `evaluator.kind` from the actual evaluation source:

| Actual source | `evaluator.kind` |
|---|---|
| Task-native verifier | `task_native` |
| Human reviewer | `human` |
| Private enterprise evaluator | `enterprise` |
| Agentic judge | `agentic` |
| Genuinely uncategorized evaluator | `generic` |

`config_digest` is a SHA-256 digest of the fixed evaluator rubric and credit
policy. Keep evaluator identity, configuration digest, and idempotency key
stable for an equivalent retry.

`confidence_ppm` represents the evaluator's confidence that its `verdict` is
correct. It is not a quality score, star rating, rubric score, or conversion of
an illustrative value. Use `null` when the evaluator or fixed rubric does not
supply verdict confidence.

Metric ids must be lowercase namespaced identifiers. Units have exact values:

| Unit | Valid values |
|---|---|
| `boolean` | `0` or `1` |
| `ppm` | Integer `0..1000000` |
| `micro_usd` | Non-negative integer USD micro-units |
| `milliseconds` | Non-negative integer milliseconds |
| `count` | Non-negative integer |
| `scalar_micros` | Signed integer scalar micro-units |

The current route compiler consumes credited `quality.pass` (`boolean`),
`cost.usd_micros` (`micro_usd`), `latency.ms` (`milliseconds`), and credited
hard violations. Other requested metrics remain immutable records but do not
become route evidence. Submit only requested dimensions; absence means
unsupported, while zero is an observed value.

For optimizer quality and cost gates, only complete `task` or `episode`
subjects qualify. Submit `cost.usd_micros` as the cost of the complete task or
episode, not one request. BitRouter prefers complete trajectory cost when it is
available and otherwise accepts evaluator-authored complete cost with the
`micro_usd` unit. Request subjects can rank optimization opportunities but do
not enter the quality or cost gate.

`inconclusive` is a quality firewall. Even if an old or malformed packet gives
positive credit to `quality.pass` or a hard violation, the route compiler does
not increase eligible episodes, independent tasks, pass/fail weight, total
quality weight, or critical-violation evidence. Explicitly credited cost and
latency remain independent observations and may aggregate.

For one decision, omitted `decision_credit` means full credit. To deliberately
withhold attribution from a one-decision inconclusive result, include that
decision with `weight_ppm: 0`; an empty map would activate implicit full credit.
For two or more decisions, use the fixed evaluator credit policy:

| Policy result | Concrete `decision_credit` |
| --- | --- |
| Exact decision/metric mappings are supported | Emit only those configured mappings and weights. |
| No policy or no exact mapping | Use `{}` or omit the serde-defaulted field. No decision receives credit, so the result produces no per-route evidence. |

For emitted mappings, `weight_ppm` is `0..1000000`; `metric_ids` must name a
metric in `metrics`, a declared hard violation, or `quality.pass`. An empty
`metric_ids` applies that credit to every present metric, so use it only when
the fixed policy intentionally attributes every present metric. Keep
hypothetical or illustrative weights outside submit-ready JSON.

A task- or episode-level terminal score is not automatically request-level
evidence. Do not duplicate it across the subject's decisions. A fixed
counterfactual policy may credit a route family when a matched baseline differs
only on that family; shared candidate/baseline failure is inconclusive, not
negative evidence. If several route families changed and the evaluator cannot
separate their effects, submit no positive-weight decision credit.

When a provider or transport request fails but the task later obtains an
authoritative terminal reward, preserve that terminal outcome as the task
record. Withhold quality credit from the failed request. If contamination makes
the task-to-route attribution ambiguous, use explicit zero credit for a
single-decision result or no positive mapping for a multi-decision result;
reliability, cost, and latency stay separate.

## Authority, submission, and ownership

Authenticated REST evaluators must match an operator-configured authority:

```yaml
eval:
  authorities:
    task-verifier:
      kind: task_native
      api_key_ids: [brvk_ci]
      user_ids: []
      allowed_metrics: [quality.*, cost.*, latency.*]
      allow_hard_fail: false
```

`kind` must match the submitted evaluator kind. `api_key_ids` or `user_ids`
must bind the authenticated principal; `allowed_metrics` accepts exact ids,
namespace wildcards such as `quality.*`, or `*`. Keep `allow_hard_fail: false`
for a remote authority. Set it to `true` only when that authority's explicit
contract permits hard-violation reports. Local CLI submission is a local
operator action and does not consult an authority binding.

Use these CLI operations:

```bash
bro eval subject seal subject-draft.json --output subject.json
bro eval subject put subject.json --config bitrouter.yaml
bro eval subject get eval-1 --config bitrouter.yaml
bro eval subject list --config bitrouter.yaml
bro eval result submit result.json --config bitrouter.yaml
bro eval status --config bitrouter.yaml
```

The authenticated REST equivalents are:

```text
POST /v1/evals/subjects       GET /v1/evals/subjects
GET  /v1/evals/subjects/{eval_id}
POST /v1/evals/results
POST /v1/evals/snapshots      GET /v1/evals/snapshots/{evidence_root}
GET  /v1/evals/status
```

CLI rows belong to owner `local`. REST rows belong to the authenticated virtual
key's owning user. The owner is storage metadata, not a wire field; do not add
tenant fields to the JSON contract. A snapshot is owner-domain-separated and
commits subject and result content digests.

## Admission and stop conditions

The submit response reports one of these admission states:

| State | Evaluator action |
|---|---|
| `admitted` | Hand off the immutable packet as eligible evidence, then stop. |
| `held_out` | Preserve it as held-out evidence; it cannot train. |
| `rejected` | Preserve rejection evidence and correct the adapter in a new immutable result identity. |
| `disputed` | Preserve conflicting evidence; it cannot train. |

Rejection can result from a digest mismatch, unknown subject, invalid schema,
unrequested metric, unknown decision/evidence reference, unauthorized
authority, kind mismatch, metric scope, or disallowed hard violation. A
conflict with an equal or higher authority becomes `disputed`; do not average
verdicts. The evaluator stops at admission in every state: never freeze,
compile, diff, publish, or invoke `optimize run`. A later explicit
`bro optimize run` is separate authorization for one autonomous
controller step and needs no evaluator review or publication approval.

## Complete multi-decision example

This task-level packet records an independently verified quality outcome for
one decision and a settled-cost observation for another. Its fixed evaluator
credit policy permits exactly the mappings below, so they belong in the
submit-ready result.

```json
// subject-draft.json
{
  "schema_version": 1,
  "eval_id": "task-run-42",
  "scope": "task",
  "subject_id": "work-item-42",
  "policy_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "preset": "auto:cost",
  "cohort": "evaluation",
  "holdout": false,
  "decisions": [
    {"decision_id":"decision-edit","policy":"auto","route_projection":"agent_route/v1|code:generation|implement|normal","request_key":"agent_route/v1|unknown|implement|normal","selected_tier":"economy","baseline_tier":"strong","policy_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
    {"decision_id":"decision-review","policy":"auto","route_projection":"agent_route/v1|code:review|verify|normal","request_key":"agent_route/v1|code:review|verify|normal","selected_tier":"strong","baseline_tier":"strong","policy_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
  ],
  "requested_dimensions": ["quality.pass", "cost.usd_micros", "latency.ms"],
  "evidence": [
    {"evidence_id":"verifier","kind":"task.verifier","digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","redacted":true,"attributes":{"result":"pass"}},
    {"evidence_id":"settlement","kind":"billing.settlement","digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","redacted":true,"attributes":{"state":"settled"}}
  ],
  "evidence_digest": "",
  "observed_at": "2026-07-31T00:00:00Z"
}
```

```bash
bro eval subject seal subject-draft.json --output subject.json
```

```json
// result.json; copy evidence_digest from subject.json
{
  "schema_version": 1,
  "eval_id": "task-run-42",
  "evidence_digest": "sha256:<digest from subject.json>",
  "evaluator": {"authority_id":"task-verifier","evaluator_id":"verify-v2","kind":"task_native","version":"2","config_digest":"sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"},
  "verdict": "pass",
  "metrics": {
    "quality.pass": {"value":1,"unit":"boolean"},
    "cost.usd_micros": {"value":420,"unit":"micro_usd"},
    "latency.ms": {"value":315,"unit":"milliseconds"}
  },
  "hard_violations": [],
  "confidence_ppm": null,
  "evidence_refs": ["verifier", "settlement"],
  "decision_credit": {
    "decision-edit": {"weight_ppm":1000000,"metric_ids":["quality.pass"]},
    "decision-review": {"weight_ppm":1000000,"metric_ids":["cost.usd_micros", "latency.ms"]}
  },
  "idempotency_key": "task-run-42-v2",
  "submitted_at": "2026-07-31T00:05:00Z"
}
```

Insert `subject.json`, submit `result.json`, require `admitted`, and hand the
packet to the operator. Do not publish a policy from this evaluator workflow.
