# BitRouter Eval Exchange Reference

Evaluate outcomes in a client or external evaluator. The service validates,
stores, queries and freezes facts; it does not run an evaluation job or publish
Policy automatically. See the [Eval contract](../../../docs/specs/eval/README.md)
for payloads and the [Service API](../../../docs/specs/service-api/README.md#eval-exchange-api)
for endpoints, authorization and retry semantics.

## Subject identity and sealing

Set `spec_version: "eval/1.0.0-beta.1"` on each subject and result. A frozen
snapshot carries the same required version. Subjects use one closed `identity`:

| `identity.profile` | Required identity | Endpoint |
| --- | --- | --- |
| `tenant_model_policy` | `installation_id`, `tenant_id`, `configuration_revision`, full `policy` ResourceRef and literal `name` | Tenant Eval |
| `tenant_generic` | `installation_id`, `tenant_id`; no Policy fields | Tenant Eval |
| `gateway_generic` | No tenant, configuration or Policy fields | Gateway Eval |

A model-policy identity may include `source_digest` and `classifier_digest` as
additional evidence. Digests do not replace resource revisions or grant authority.
A tenant profile may include `execution: {execution_id, configuration_revision}`;
the service verifies the original receipt in that tenant. Do not derive execution
identity from a correlation ID or a caller-assigned subject ID.

The service checks model-policy tuples against the authenticated immutable
configuration and its real route binding. Preserve the literal selected name:
`auto` and `auto:cost` are separate names. A Policy-free tenant evaluation uses
`tenant_generic`. Generic evidence cannot justify a different Policy revision.
Tenant profiles are rejected by Gateway Eval, and gateway profiles are rejected
by Tenant Eval. Mixed or unknown identity fields are rejected.

Common subject fields retain these meanings:

| Field | Meaning |
| --- | --- |
| `eval_id` | Immutable evaluation-attempt identity. |
| `subject_id` | Logical request, episode or task identity in its explicit namespace. |
| `scope` | `request`, `episode`, or `task`; select the outcome boundary actually observed. |
| `cohort` | Optional evaluator metadata. |
| `holdout` | Held-out facts remain outside admitted update evidence. |
| `decisions` | Router-authored references; empty when no exact decision attribution is available. |
| `requested_dimensions` | Namespaced metric IDs the evaluator may submit. |
| `evidence` | Redacted, content-addressed evidence items with safe string attributes. |
| `evidence_digest` | Canonical evidence digest; let the sealing command calculate it. |
| `observed_at` | RFC 3339 observation timestamp. |

Use `request` for a request-local outcome, `episode` for a bounded workflow or
conversation, and `task` for an externally defined task with a terminal task or
verifier outcome. Multiple decisions alone do not establish task scope. Give
repeated attempts distinct evaluation/result/evidence identities while keeping
the logical task identity stable within its run and source namespace.

Copy decision fields from router evidence, including exact selected/baseline
tiers and efforts, `route_projection`, `request_key`, diagnostic `policy_digest`,
and any route measurement. Both route classifications are exact
[Policy class keys](../../../docs/specs/policy/README.md#model-selection), such
as `code:debugging|verify|guarded`. A measured candidate identifies its tier,
model, optional effort and logging probability. These probabilities describe
the router's distribution. Record task quality from the observed outcome.

Keep raw prompts, code, tool arguments and evaluator output in the evaluator's
private source. Evidence items contain `evidence_id`, namespaced `kind`, a
`sha256:<64 lowercase hex>` digest, `redacted: true`, and optional safe attributes.
Never include credentials or raw private material.

Seal a JSON or YAML draft locally:

```bash
bitrouter eval subject seal subject-draft.json --output subject.json
```

The command sorts evidence, computes the canonical digest, validates the subject
and writes deterministic JSON. It does not open the ledger or grant tenant authority.

## Result contract and units

The result inherits identity from the validated subject:

```text
spec_version: "eval/1.0.0-beta.1"
eval_id: exact sealed subject ID
evidence_digest: exact sealed evidence digest
evaluator: { authority_id, evaluator_id, kind, version, config_digest }
verdict: pass | fail | inconclusive
metrics: { metric_id: { value, unit } }
hard_violations: [metric_id]
confidence_ppm: integer 0..1000000 or null
evidence_refs: [subject evidence_id]
decision_credit: { decision_id: { weight_ppm, metric_ids } }
idempotency_key: stable nonempty identifier
submitted_at: RFC 3339 timestamp
```

For gateway authorities, use the evaluator kind that matches the configured
source: `task_native`, `human`, `enterprise`, `agentic`, or `generic`. The initial
tenant manual profile requires `kind: generic`, the authenticated principal's
exact `authority_id`, and no `hard_violations`. Host EvalConfig cannot grant a
tenant evaluator trusted rank or permission.

`config_digest` describes the fixed evaluator rubric and credit policy. Preserve
it, evaluator identity, evidence and idempotency key for an equivalent retry.
`confidence_ppm` measures confidence that the verdict is correct; it is not a
quality score. Use `null` when the rubric supplies no verdict confidence.

| Unit | Valid value |
| --- | --- |
| `boolean` | `0` or `1` |
| `ppm` | Integer `0..1000000` |
| `micro_usd` | Nonnegative integer USD micro-units |
| `milliseconds` | Nonnegative integer milliseconds |
| `count` | Nonnegative integer |
| `scalar_micros` | Signed integer scalar micro-units |

Submit only requested metrics. Absence means unsupported; zero is an observed
value. For task/episode cost, report the complete unit cost, not a single request's
price. Transport success or low cost does not establish task success. Use
`inconclusive` when evidence does not justify pass or fail.

Decision credit must follow a fixed, evidence-supported attribution policy.
For one decision, empty credit means implicit full credit; use an explicit
zero weight to withhold attribution. For multiple decisions without an exact
mapping, use `{}`. Positive weights range up to 1,000,000 ppm and may name only
submitted metrics, declared violations or `quality.pass`. An empty `metric_ids`
set applies credit to every present metric. Do not duplicate a terminal task
reward across all its request decisions. Cost, latency and reliability remain
separate observations from task quality.

## Authenticated submission and ownership

Gateway Eval uses the gateway credential boundary and its configured evaluator
authorities. For example:

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

Kind, principal binding, metric scope and hard-violation permission must match.
Local CLI submission is an explicit local operator operation; it does not confer
remote tenant authority. Current CLI operations address that gateway ledger:

```bash
bitrouter eval subject put subject.json --config bitrouter.yaml
bitrouter eval subject get eval-1 --config bitrouter.yaml
bitrouter eval subject list --config bitrouter.yaml
bitrouter eval result submit result.json --config bitrouter.yaml
bitrouter eval status --config bitrouter.yaml
```

Gateway endpoints use `/v1/evals`. Tenant endpoints use
`/control/v1/tenants/{tenant_id}/evals`, with a tenant bearer credential and exactly
one `Bitrouter-Installation-Id` header. The body identity and addressed namespace
must agree. The SDK exposes these through `GatewayClient` and `TenancyClient`
respectively.

Both offer subjects (POST/GET), subjects/{eval_id} (GET), results (POST),
snapshots (POST), snapshots/{evidence_root} (GET), and status (GET).
Tenant subject/result submission requires `evaluation.submit`; reads require
`resource.read`; freeze requires both. Results cannot reference another tenant's
subject. A frozen snapshot commits immutable subject and result content, including
the subject's complete identity. Query and freeze do not activate a Policy.

An interrupted mutation is not proof of rollback. Preserve the packet and
recover its observed state; never treat a network failure as permission to
rewrite it or retry with different identities.

## Admission and explicit update

| Status | Handling |
| --- | --- |
| `admitted` | Preserve the immutable packet and admission receipt. |
| `held_out` | Preserve held-out facts outside update evidence. |
| `rejected` | Preserve rejection evidence; correct the adapter with a new immutable result identity. |
| `disputed` | Preserve conflicting facts; do not average verdicts. |

An evaluator's authorized handoff ends after submission unless the task also
explicitly requests freezing or updating Policy.

The explicit client-driven update flow is: infer against an exact retained
configuration, evaluate its actual output, submit/freeze the bound facts, then
independently author a public `policy::Document`, create immutable resource and
configuration revisions, and activate with the required authority and CAS.
Result admission, freeze and a draft never change the active configuration.
Keep original facts bound to their original tuple after activation.

## Gateway packet example

This gateway packet deliberately claims no tenant Policy authority. Fill the
redacted verifier digest with a digest of actual private evidence, then seal it:

```json
{
  "spec_version": "eval/1.0.0-beta.1",
  "identity": {"profile": "gateway_generic"},
  "eval_id": "task-run-42",
  "scope": "task",
  "subject_id": "work-item-42",
  "cohort": "evaluation",
  "holdout": false,
  "decisions": [],
  "requested_dimensions": ["quality.pass"],
  "evidence": [{"evidence_id":"verifier","kind":"task.verifier","digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","redacted":true,"attributes":{"result":"pass"}}],
  "evidence_digest": "",
  "observed_at": "2026-11-18T00:00:00Z"
}
```

A corresponding result uses the same `spec_version`, repeats the sealed `eval_id` and
`evidence_digest`, uses its actual authorized evaluator identity, and cites
`verifier`. For model-policy evidence, instead supply the complete authenticated
`tenant_model_policy` tuple and submit through Tenant Eval.
