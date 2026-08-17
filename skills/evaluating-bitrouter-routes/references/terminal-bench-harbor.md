# Harbor and Terminal-Bench Route Evidence

Use this reference only for Harbor/Terminal-Bench artifacts. BitRouter receives
the resulting generic Eval Exchange packets; every benchmark-specific join,
exception rule, and screening rule stays in this external skill.

## Contents

- [Inputs and exact joins](#inputs-and-exact-joins)
- [Request error taxonomy](#request-error-taxonomy)
- [Strict quality attribution](#strict-quality-attribution)
- [Observational screening is not Eval evidence](#observational-screening-is-not-eval-evidence)
- [Output contract](#output-contract)
- [Current calibration evidence](#current-calibration-evidence)

## Inputs and exact joins

Run:

```bash
python3 scripts/terminal_bench_route_evidence.py \
  --run-dir /path/to/harbor-run \
  --decisions /path/to/policy-decisions.jsonl \
  --traces /path/to/traces.jsonl \
  --request-outcomes /path/to/current-request-cost-join.jsonl \
  --output-dir /path/to/evolution-analysis
```

The run directory contains Harbor `result.json` files and each trial's
`agent/trajectory.json`. Raw trace rows contain the physical request id and
request messages. Decision rows contain BitRouter's
`ingress_request_id_sha256` and router fields. Request-outcome rows use the
authoritative run schema: unique physical `request_id`, an explicit nullable
`error`, and optional cost, token, and latency/timestamp fields.

The adapter accepts exactly two trace-to-task identities:

1. The trace messages equal one complete Harbor trajectory prefix ending at a
   user message.
2. When the full prefix differs, a hashed `Task Description` field uniquely
   identifies one trial.

It then joins trace to decision through:

```text
sha256("bitrouter.ingress-request-id.v1\0" + physical_trace_request_id)
```

The task and ingress joins plus the trace-to-request-outcome join must all be
unique and complete. Capture timestamps and agent-execution windows are never
join keys. Duplicate task descriptions, duplicate ids/rows, missing
trajectories, unconsumed inputs, and unmatched traces fail closed. A defect
uniquely attributable to a trial blocks that canonical task; a defect that
cannot be assigned uniquely blocks the entire batch. Inspect the task-local and
global coverage reasons in `join-summary.json`; never repair ambiguity by
proximity. The adapter accepts the router log's durable `request_id` as the
decision identity when a separate `decision_id` field is absent, but never
fabricates one from position or time.

## Request error taxonomy

Rules are ordered. Specific status rules win over broad text:

| Order | Category | Rule id | Evidence |
|---:|---|---|---|
| 1 | auth | `auth.http-401-403.v1` | HTTP 401 or 403 |
| 2 | rate limit | `rate-limit.http-429.v1` | HTTP 429 |
| 3 | auth | `auth.credentials.v1` | explicit invalid-credential marker |
| 4 | rate limit | `rate-limit.marker.v1` | explicit rate-limit marker |
| 5 | network | `network.dns.v1` | DNS/name-resolution marker |
| 6 | transport | `transport.connection-reset.v1` | reset, broken pipe, or authoritative upstream-timeout field |
| 7 | provider | `provider.policy-violation.v1` | authoritative upstream policy field |
| 8 | provider | `provider.upstream-unavailable.v1` | authoritative upstream-unavailable field |

Only field-level matches are non-task exclusions. Do not infer a provider error
from task text or a verifier failure. In particular, Harbor
`AgentTimeoutError` is not automatically non-task; it stays inconclusive unless
a request field independently matches the taxonomy.

A binary terminal verifier reward wins as the task outcome. If a request failed
and the task later receives a reward, retain pass/fail at task level while the
failed request contributes no quality credit. Reliability counts and separately
observed cost/latency remain available.

## Strict quality attribution

One task produces at most one positive quality mapping. Require all conditions:

1. terminal verifier reward is exactly 0 or 1;
2. all task requests have unique exact task, decision, and ingress joins;
3. all requests use one policy and one route projection;
4. all requests use one actual selected tier;
5. the task has zero non-task request contamination.

Choose the earliest `(captured_at, decision_id)` as the one representative and
credit only it at 1,000,000 ppm. Never copy the reward onto all requests. Mixed
route cells, mixed tiers, later strong recovery, missing ids, or ambiguous joins
produce no positive quality mapping. For a recovered single-decision task,
preserve the terminal verdict with explicit zero quality credit.

The direct economy gate is stricter still. A route needs actual economy use,
five independent conclusive uniquely attributed tasks, pass rate at least 80%,
explicitly known zero critical violations, zero later-strong recovery dependency, zero
attribution ambiguity, and zero non-task contamination. Only this layer may set
`active_recommendation: economy`.

Later-strong recovery is calculated once across the complete ordered trial and
propagated to every cell that trial touched. Critical violations are read only
from an explicit result/eval artifact field. A missing field is unknown, never
zero, and blocks both active and controlled-validation recommendations.

## Observational screening is not Eval evidence

Mixed-treatment tasks can still identify where a controlled experiment would
be useful. This second layer associates terminal task outcomes with every
exact-matched route cell exercised, but it is non-causal and always separate
from Eval quality credit.

A cell is a controlled-validation candidate only when it has complete request
coverage, actual economy
exposure or is balanced at normal risk, at least five distinct terminal-pass
associations, zero terminal-fail associations, zero route-level non-task
errors, zero recovery dependency, and explicitly known zero critical
violations.

The matrix emits:

- `quality_credit_eligible`: strict layer only;
- `active_recommendation`: strict gate only;
- `controlled_validation_candidate`: observational layer only;
- `screening_reason`: the passed gate or exact failure reason;
- `economy_experiment_candidate`: a screened balanced/normal cell only.

Controlled-validation candidates carry zero training credit, are not safety
claims, and cannot publish or automatically edit the template.

## Output contract

`packets.jsonl` contains generic schema-v1 subject/result pairs. Validate each
subject with `bitrouter eval subject seal`, submit through the normal Eval
Exchange, and stop after admission. Operators own snapshots, candidates,
diffs, and publication.

`task-evidence.jsonl`, `matrix.json`, and `matrix.csv` are external audit data.
Matrix rows include route projection, task family, role, risk, independent
tasks/episodes, pass/fail/inconclusive, selected/static tier distributions,
cost, latency, guard promotion, non-task exclusions/rule ids, ambiguity,
coverage failures, recovery, known/unknown critical violations, evidence grade,
and both recommendation layers. CSV text is spreadsheet-formula escaped.

Eval and idempotency identities bind the adapter and taxonomy versions, run
identity, complete input/source digest, canonical task, trial/result attempt,
and attribution digest. Equivalent reruns are stable; separate run/attempt
evidence cannot collide. Matrix independent-task counts still deduplicate the
canonical task, while independent episodes retain attempt identity.

## Current calibration evidence

The 2026-08-17 offline calibration covered 89 formal tasks and 1,306 exact
formal request/decision/cost joins. It excluded 303 non-task request failures:
216 upstream unavailable, 45 upstream policy violations, and 42 upstream
timeouts. All 89 tasks were strict-quality inconclusive because each whole task
mixed route cells or selected tiers; therefore there were zero active-safe
economy routes and zero qualified economy experiment candidates. Seven balanced
normal cells remained in the screening-all report with their failed gates, not
as candidates.

Changing only progress protection from `[strong]` to `[strong, balanced]`
reclassified the formal logged decisions to 351 strong, 915 balanced, and 40
economy; the all-run accounting was 351/917/40. This is static accounting, not
a counterfactual quality result. The private artifact is intentionally not
shipped; its `sha256-manifest.json` digest is
`2adb125f71b8e362984894281710a30e75c27f3d7b45f8ca74a3106ad930403e`.

The v2 adapter replay over the complete raw inputs reports 1,306 exact joins,
303 exclusions (261 provider and 42 transport), and three unmatched traces plus
two unconsumed decisions outside the formal request-outcome set. Those
unattributable extras correctly trigger the global quality block. The replay
therefore emits zero quality-eligible matrix rows, zero active economy routes,
and zero controlled-validation candidates.
