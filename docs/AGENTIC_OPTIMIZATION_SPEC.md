# History-Driven Agentic Optimization

Status: implementation contract

## Product outcome

BitRouter evolves a named routing policy from traces and evaluation results
produced by normal agent use. The optimizer does not launch workflows, create
Git worktrees, run private daemons, replay workloads, or bundle an evaluator.
Evaluators submit immutable results through the generic Eval Exchange or the
evaluating-routes skill.

The objective is constrained cost minimization:

```text
minimize expected task/episode cost
subject to the candidate cohort passing its Eval gate
```

Calling `bitrouter optimize run` grants one controller step authority to
publish its decision. Review is an audit surface, not an approval gate. The
controller may start exploration, promote a challenger, retreat to the last
known good route, continue gathering evidence, or report convergence.

## CLI contract

```text
bitrouter optimize run \
  [--policy auto] \
  [--candidate-tier economy] \
  [--exploration-ppm 100000] \
  [--minimum-tasks 3] \
  [--maximum-tasks 20] \
  [--minimum-pass-rate-ppm 900000] \
  [--evaluator-config-digest sha256:...] \
  [--config bitrouter.yaml] \
  [--socket PATH]

bitrouter optimize status [--policy auto] [--config bitrouter.yaml]
```

`run` performs one deterministic transition from the active policy and the
currently admitted local Eval material. It publishes the successor atomically
and reloads a reachable daemon. It never runs a benchmark. An invocation on a
frozen policy explicitly activates adaptive mode as part of the recoverable
publication operation.

The previous `setup`, `resolve`, `review`, `publish`, and `rollback` workflow
experiment commands are removed. `bitrouter.optimize.yaml`,
`bitrouter.optimize.lock.yaml`, and `bitrouter.eval.md` are no longer created or
consumed. Existing generic Eval and low-level policy compile/diff/publish
interfaces remain supported.

## Signed exploration state

Each named policy may carry an active optimization experiment and a bounded
rejection ledger in the signed policy lock. An experiment records:

- its deterministic experiment id;
- one target request key;
- exact champion and challenger tiers;
- challenger exposure in parts per million;
- minimum independent tasks per arm and maximum challenger tasks;
- minimum candidate pass rate; and
- an optional evaluator configuration digest.

The experiment id and assignment salt derive from the parent policy digest,
policy name, route key, treatments, and gate settings. A rejection binds the
same treatment context to its immutable evidence root and reason. BitRouter
does not retry an unchanged rejected context. Policy history remains the full
transition log.

All new lock fields are optional, so existing policy locks remain readable.
Validation bounds exposure and pass rates to `1..=1_000_000`, requires positive
sample budgets, verifies the target tiers, and caps rejection history at 256
records.

## Task- and episode-level assignment

Exploration changes exactly one route key. Assignment prefers benchmark run
plus trial identity and otherwise uses the stable parent workflow/session
identity produced by the trace adapter. If no stable task or episode identity
exists, routing fails safe to the champion arm.

The router hashes the experiment id with this stable identity. Every use of
the target key in the same task receives the same arm. Tool and progress
guardrails run after assignment and may clamp the actual treatment upward.

Router-authored decision evidence gains an optional experiment reference with
the experiment id, `control` or `challenger` arm, `task` or `episode`
assignment unit, a redacted assignment-id digest, and challenger propensity.
This optional field is backward compatible with Eval schema v1. Optimizer
cohort membership comes only from signed router evidence, never from the
evaluator-owned `cohort` string.

## Cold start

Champion-only history cannot prove an unexecuted challenger. It can only show
that the champion is currently feasible and rank what to explore next.

When no experiment is active, the controller ranks eligible route keys by
observed request cost contribution, independent task/episode coverage, and
canonical key order. It excludes operator-owned routes, already-cheap routes,
guarded-only treatments, and unchanged treatment contexts already rejected.
The successor leaves champion routes unchanged and activates fractional live
exploration for the highest-ranked key.

Request evidence is used only for exploration priority. It is never promotion
evidence.

## Cohort gate

For an active experiment, the controller freezes an immutable admitted Eval
snapshot and filters it by exact experiment policy digest and experiment id.
Only task and episode subjects enter the quality cohort. Subjects with mixed or
contradictory arm references are excluded and reported.

Conclusive challenger results are grouped by unique subject identity. An
explicit evaluator configuration digest pins the gate. Without one, exactly
one conclusive evaluator configuration must be present. Duplicate conflicting
results make a subject ineligible.

The candidate gate passes when:

- both arms have the configured minimum independent subjects;
- candidate pass rate meets the configured minimum; and
- no candidate hard violation exists.

Cost uses complete task or episode cost, preferring
`trajectory.cost.usd_micros` and accepting evaluator-authored
`cost.usd_micros` with the correct unit. Promotion additionally requires lower
candidate mean cost than control mean cost.

The state machine is:

- **promote:** the gate passes and candidate cost is lower; set the route to
  the challenger and clear exploration;
- **retreat:** any hard violation, or the sample budget is exhausted without
  satisfying quality and cost; preserve champion semantics, clear exploration,
  and record the rejection;
- **hold:** evidence remains inconclusive within budget; do not rewrite the
  policy lock; and
- **converged:** no eligible unrejected treatment remains; do not rewrite the
  policy lock.

Retreat publishes a new descendant rather than restoring old bytes.

## Publication and audit

Only the active policy lock controls routing. Database state may trigger a
controller decision but never changes routing directly.

Every mutating step acquires the existing publication lock, reloads the active
policy, uses its digest as the compare-and-swap parent, publishes through the
atomic policy-history path, and reloads the daemon when reachable. A stale
parent fails closed. Config activation, policy publication, and daemon reload
are recovered together on failure.

The policy artifact binds the parent digest, immutable Eval root,
observed-subject input digest, and history-optimizer compiler configuration.
Certificates remain content-free and record eligible task counts, pass rate,
hard violations, complete-cost delta, evaluator configuration, evidence
digest, and the experiment, promotion, or blocked verdict.

Reports expose the controller decision, policy and evidence digests, target
key and treatments, exposure and budgets, eligible/excluded cohort counts,
pass/fail/hard-violation totals, cost means and delta, evaluator configuration,
and publication/reload outcome. They never contain prompts, model output, tool
arguments, evaluator output, task answers, credentials, or repository paths.

## Removal contract

Delete the private experiment runner, worktree manager, private daemon/database
lifecycle, embedded ACP evaluator, workflow discovery, optimization intent and
lock formats, workflow-optimization onboarding, and optimization-specific
rollback. Existing optimization intent files become inert user-owned files.

The implementation is complete when tests prove deterministic task assignment,
safe control fallback, guardrail clamping, champion-only cold start,
task/episode-only gates, complete-cost promotion, promotion/retreat/hold/
convergence, rejection deduplication, stale-parent protection, mode/reload
recovery, removal of the paired workflow surface, and full CLI/skill
documentation consistency.
