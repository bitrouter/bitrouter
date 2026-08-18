# Harbor and Terminal-Bench Route Evidence

Use this reference only for Harbor/Terminal-Bench artifacts. BitRouter receives
the resulting generic Eval Exchange packets; every benchmark-specific join,
exception rule, and screening rule stays in this external skill.

## Contents

- [Inputs and exact joins](#inputs-and-exact-joins)
- [Request error taxonomy](#request-error-taxonomy)
- [Strict quality attribution](#strict-quality-attribution)
- [Observational screening is not Eval evidence](#observational-screening-is-not-eval-evidence)
- [Strong-tier cost-budget planning](#strong-tier-cost-budget-planning)
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

Every explicit non-null authoritative request error is retained. When no rule
matches, the adapter records category `null` and rule `unclassified.v1` instead
of silently treating the request as clean or misreporting a known provider
class. `unclassified_request_errors` and `unclassified_error_rule_ids` are
separate from the known exclusion counts. Any such error blocks quality and
both recommendation layers for every cell associated with its affected task;
`unclassified_contaminated_tasks` exposes that propagated task count without
duplicating the physical request-error count.

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
5. the task has zero known non-task request contamination and zero unclassified
   request errors.

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
violations. Unclassified request errors also block screening; they never become
passing observational associations.

The matrix emits:

- `quality_credit_eligible`: strict layer only;
- `active_recommendation`: strict gate only;
- `controlled_validation_candidate`: observational layer only;
- `screening_reason`: the passed gate or exact failure reason;
- `economy_experiment_candidate`: a screened balanced/normal cell only.

Controlled-validation candidates carry zero training credit, are not safety
claims, and cannot publish or automatically edit the template.

## Strong-tier cost-budget planning

This planner is separate from strict quality attribution and observational
screening. Use it only after an operator has frozen a task cohort whose
validity definition excludes terminal exceptions and matched physical request
errors.

```bash
python3 scripts/terminal_bench_strong_tier_plan.py \
  --validity-audit /path/to/validity-audit.json \
  --request-join /path/to/current-request-model-join.jsonl \
  --daemon-log /path/to/bitrouter-daemon.log \
  --control-attempt-cost 12.34 \
  --control-attempt-cost 13.45 \
  --control-anchor cheapest \
  --target-policy-key 'agent_route/v1|unknown|mechanical|guarded' \
  --strong-rates '5,0.5,6.25,30' \
  --target-savings-min-percent 40 \
  --target-savings-max-percent 50 \
  --output-dir /path/to/strong-tier-plan
```

The validity audit supplies `fully_clean_tasks`. For those tasks, the request
join must contain only unique, provider-reported, priced requests with
`error: null`. The daemon log must contain exactly one policy decision for
each `trajectory_request_id`. Missing and duplicate joins fail closed.

For the requested matched policy key, the planner holds these observed token
categories fixed:

1. uncached input;
2. cache read;
3. cache write;
4. completion, including reasoning.

It replaces only their per-million-token rates with the supplied strong rates.
Reasoning is already inside completion and is never priced again. Requests on
other cells retain their recorded nominal cost. This is an observed-token
repricing counterfactual, not a prediction of strong-model token behavior,
reward, or settled cash cost.

Every exact-case control attempt stays on its own row. The planner identifies
the cheapest attempt, but it becomes the target-band anchor only when the
operator has explicitly approved that conservative rule. Other attempts remain
sensitivity evidence. This avoids inventing an aggregate and makes an empty
intersection across control attempts visible.

The deterministic outputs are:

- `summary.json`: strict counts, route shares, candidate cost, each control,
  and the conservative anchor;
- `route-cells.csv`: matched-key request/task counts and current/candidate
  costs;
- `report.md`: human-readable method and separate control rows;
- `sha256-manifest.json`: hashes of the preceding outputs.

The artifact supports an operator-reviewed generic policy change. It carries
zero Eval quality credit, cannot set `active_recommendation`, and cannot
publish or edit a policy by itself.

## Output contract

`packets.jsonl` contains generic schema-v1 subject/result pairs. Validate each
subject with `bitrouter eval subject seal`, submit through the normal Eval
Exchange, and stop after admission. Operators own snapshots, candidates,
diffs, and publication.

`task-evidence.jsonl`, `matrix.json`, and `matrix.csv` are external audit data.
Matrix rows include route projection, task family, role, risk, independent
tasks/episodes, pass/fail/inconclusive, selected/static tier distributions,
cost, latency, guard promotion, non-task exclusions/rule ids, ambiguity,
separate unclassified-error counts/rule ids, coverage failures, recovery,
known/unknown critical violations, evidence grade, and both recommendation
layers. CSV text is spreadsheet-formula escaped.

Eval and idempotency identities bind the adapter and taxonomy versions, run
identity, complete input/source digest, canonical task, trial/result attempt,
and attribution digest. Equivalent reruns are stable; separate run/attempt
evidence cannot collide. Subject identity is deliberately separate: it binds
canonical task plus run, complete source, and policy namespace, excluding the
adapter/taxonomy versions and trial/result attempt. Repeated attempts therefore remain separate eligible
episodes but the generic compiler and matrix both count one independent task.
Unrelated run/source/policy namespaces do not collide.

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

The v3 adapter replay over the complete raw inputs reports 1,306 exact joins,
303 exclusions (261 provider and 42 transport), and three unmatched traces plus
two unconsumed decisions outside the formal request-outcome set. Those
unattributable extras correctly trigger the global quality block. The replay
has zero unclassified request errors and therefore emits zero quality-eligible
matrix rows, zero active economy routes, and zero controlled-validation
candidates.
