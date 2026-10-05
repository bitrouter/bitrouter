# History-driven routing optimization

Use this flow when the user wants to reduce the complete cost of agent tasks or
episodes while retaining an externally evaluated quality threshold. BitRouter
does not launch a benchmark, discover a workflow, manage worktrees, or bundle a
judge. Normal routed use produces traces; an external evaluator submits generic
Eval Exchange results; the optimizer advances the signed policy one step.

## Initialize and collect history

Create a named adaptive policy with a strong champion and an economy tier:

```bash
bro policy init auto --preset auto --economy provider:model
```

Then run the coding agent or Terminal Bench normally through `bitrouter/auto`.
The whole `bitrouter/` namespace is reserved and resolved before provider
lookup; an unknown slug is a `400`. If `bitrouter/auto` has no binding, create
one with `policy init`; BitRouter never falls through to a provider default.

Route requests must carry a stable task or episode identity for experimental
assignment. BitRouter prefers benchmark run plus trial identity, then a parent
workflow/session identity. Missing stable identity fails safe to the champion.
Every target-route request in the same assignment unit gets the same arm.
Tool-use and progress guardrails may clamp the actual route upward without
changing the router-authored arm evidence.

## Evaluate outside the serving path

Use the `evaluating-bitrouter-routes` skill or another generic Eval adapter to
seal subjects and submit immutable results:

```bash
bro eval subject seal subject-draft.json --output subject.json
bro eval subject put subject.json --config bitrouter.yaml
bro eval result submit result.json --config bitrouter.yaml
```

The evaluator stops after admission. It never freezes snapshots, compiles a
candidate, or publishes a policy. It must preserve each router-authored
decision reference, including the optional `experiment` object, verbatim. It
must never invent or edit the experiment id, arm, assignment unit,
assignment-id digest, or challenger propensity. The evaluator-owned `cohort`
string does not determine optimizer membership.

Only complete `task` and `episode` subjects gate optimization quality and cost.
Request subjects rank opportunities by observed frequency and cost
contribution, but cannot pass or fail a challenger. Record the complete task or
episode cost: prefer `trajectory.cost.usd_micros`; evaluator-authored
`cost.usd_micros` must use the `micro_usd` unit. Per-request price is not the
promotion cost.

## Advance one autonomous step

```bash
bro optimize run --policy auto --config bitrouter.yaml
bro optimize status --policy auto --config bitrouter.yaml
```

Calling `optimize run` is authorization for exactly one autonomous controller
step. There is no manual review or publish approval. The command reads an
immutable admitted-Eval snapshot, compares against the active policy digest,
publishes a successor atomically when the decision mutates state, and reloads a
reachable daemon. A concurrent publisher makes the parent stale. Publication
or reload failure restores the previous active state.

The first run can use champion-only history to rank a route and cold-start
signed exploration; it cannot promote an unexecuted challenger. Later steps
are:

- `promote`: both arms have enough complete independent subjects, the
  challenger meets the pass-rate gate with no hard violation, and its mean
  complete-unit cost is lower;
- `retreat`: a hard violation occurs, or the challenger budget is exhausted
  without satisfying quality and cost;
- `hold`: evidence remains inconclusive within budget;
- `converged`: no eligible unrejected route/treatment remains.

Repeat normal traced agent or Terminal Bench use, external Eval submission, and
`optimize run` until that command reports `converged`. `optimize status` is an
optional, database-read-only observation of the signed policy: it reports
`exploring` when an experiment is active and `idle` otherwise. Idle does not
prove convergence because promotion and retreat also clear the active
experiment. A retreat records its treatment context so the same rejected
experiment is not retried unless its tier target or gate changes.

Controller flags tune exposure and gates when needed:

```text
--candidate-tier TIER
--exploration-ppm 100000
--minimum-tasks 3
--maximum-tasks 20
--minimum-pass-rate-ppm 900000
--evaluator-config-digest sha256:...
```

Omit `--candidate-tier` to use the signed policy's
`adequacy.explore_tier`; pass `TIER` only to override that value for the step.

Without an explicit evaluator configuration digest, the candidate cohort must
contain exactly one conclusive evaluator configuration. Conflicting or
duplicate conclusive results for a subject and mixed arm evidence are excluded.

## Generic and low-level interfaces remain available

The generic Eval CLI and REST endpoints remain the evaluator interface.
`policy compile`, `policy diff`, `policy publish`, `policy rollback`, and
`policy verify --evidence` remain available for migration, audit, and explicit
operator-managed policy work. They are not required approval stages for an
optimizer step.

Reports and locks contain structural counters, digests, tier names, gate
settings, and complete-cost aggregates. Never place prompts, model output,
tool arguments, evaluator output, credentials, task answers, or repository
paths in them.
