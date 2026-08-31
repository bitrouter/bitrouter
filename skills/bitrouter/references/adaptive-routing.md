# Adaptive routing

Use the `templates/auto-router` starter when you want a conservative adaptive
policy without coupling it to an agent or workflow. Address it with the public
model slug:

```text
bitrouter/auto       # strong base policy
bitrouter/auto:cost  # same policy plus the top-level cost variant
```

`bitrouter/auto` follows the `vendor/auto` convention other gateways use, so a
config already pointing at some other `.../auto` model only needs its vendor
segment changed. The vendor segment names the router being addressed, not the
token destination — the bound policy still dispatches to whichever upstream
provider its tiers name.

The whole `bitrouter/` namespace is reserved and resolved locally, so an
unrecognised slug is a clean `400` rather than a provider lookup. Requesting
`bitrouter/auto` before a policy is bound reports the missing binding and names
`bitrouter policy init`; it does not fall back to a provider default.

The same policy is also reachable through the generic `@preset[:variant]` form
(`@auto`, `@auto:cost`) that every preset uses; `bitrouter/auto` is the
documented spelling. Explicit physical model ids remain passthrough. Copy the template, then start
the daemon with its config and validate it before serving traffic:

```bash
bitrouter config validate --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
```

`policy init` writes `policy.mode: adaptive`, allowing an explicit optimizer or
low-level publication command to replace the active lock. Routing remains
deterministic from the signed lock: Eval rows never mutate live routes on their
own. Set the process mode to `frozen` to forbid replacement while telemetry and
Eval evidence continue to accumulate. The lock itself never selects runtime
mode.

## Route inputs and safety

The starter lock uses `key_strategy: agent_trace` and its predictive table keeps
all fifteen `agent_route/v1|unknown|<role>|<risk>` baseline routes. A confident task
classification also produces the more specific
`agent_route/v1|<task-family>|<role>|<risk>` key. The twelve task-family values
are `code:generation`, `code:debugging`, `code:review`, `code:sql_database`,
`code:frontend_ui`, `code:devops_config`, `code:repository_analysis`,
`agent:multi_step_planning`, `agent:workflow_execution`, `agent:web_research`,
`agent:memory_operations`, and `agent:general`; roles are `orchestrate`,
`implement`, `mechanical`, `verify`, and `finalize`; risk is `normal`,
`context`, or `guarded`.

The official template intentionally has only three exact task-family overrides:
`code:review|verify|normal` and `code:debugging|implement|guarded` route to
`strong`, while `agent:web_research|mechanical|normal` routes to `balanced`.
For every unlisted task cell, the router uses the matching v1 unknown-family
role-and-risk baseline. Shell, file,
and tool dispatch are bounded action/role evidence only: they do not create a
task family or select a task-specific route. Task-specific cells remain
compiler-owned experiments until settled evaluation evidence promotes them.

Keep `tool_use_tier` and `tool_safe_tiers` in place when changing tier models,
so capability guardrails still apply.

Runtime adapters may parse native request formats to enrich diagnostic
evidence. They do not contribute policy keys, and no private BitRouter headers
are required for routing. `key_strategy: agent_trace` selects the deterministic
predictor; the resulting static policy keys are exclusively
`agent_route/v1|<task-family>|<role>|<risk>`. Observed `agent_trace/v2` values
remain telemetry and cannot be configured as routes. Retired route shapes and
`key_strategy: legacy_fingerprint` are rejected during validation.
`adequacy.explore_opening: true` opts
source-neutral opening projections into exploration. Do not configure the
removed `adequacy.max_downgraded_requests_per_session`; session identity is
diagnostic-only and the parser rejects that setting.

## Generic evaluation and optimization

The daemon automatically turns settled routed requests into redacted eval
subjects. It does not run a bundled judge. Task-native tests, humans,
bitrouter-agent, and private enterprise evaluators submit the same versioned
result contract through the CLI or authenticated REST API. Results are
immutable and pass authority, metric-scope, holdout, and conflict admission
before they can enter a snapshot.

Remote result writers bind their existing virtual key or user identity to an
authority in `bitrouter.yaml`:

```yaml
eval:
  authorities:
    ci:
      kind: task_native
      api_key_ids: [brvk_ci]
      allowed_metrics: [quality.*, cost.*, latency.*]
      allow_hard_fail: true
```

With `server.skip_auth: false`, the REST exchange accepts the same Bearer or
`x-api-key` credential shape as inference. Local CLI submission is treated as
an operator action.

The normal history-driven lifecycle is:

```bash
bitrouter policy init auto --preset auto --economy provider:model
# run a coding agent or Terminal Bench normally through bitrouter/auto
bitrouter eval result submit result.json --config bitrouter.yaml
bitrouter optimize run --policy auto --config bitrouter.yaml
bitrouter optimize status --policy auto --config bitrouter.yaml
```

Repeat normal traced work, external Eval submission, and `optimize run` until
that command reports `converged`. `optimize status` only observes whether the
signed policy is `exploring` or `idle`; idle does not establish convergence.
Calling `optimize run` authorizes exactly one autonomous controller step and
any atomic publication it decides; there is no manual review or publish
approval. Champion-only history can rank request opportunities and cold-start
signed exploration, but cannot promote an unexecuted challenger. Later runs
promote, retreat, hold, or converge.

Only complete `task` and `episode` cohorts gate quality and cost. Request
subjects rank opportunities only. Promotion requires the quality gate and a
lower mean complete-task or complete-episode cost, not a cheaper individual
request. Evaluators preserve the optional router-authored `experiment`
reference verbatim and never invent or edit it; the evaluator-owned `cohort`
field does not assign experiment membership.

The hot path never reads Eval rows. `frozen` and `adaptive` route identically;
mode controls write authority, not request-time learning. `bitrouter policy
verify --evidence` reconstructs an active compiled lock's evidence root when
the local ledger is available. Shipping only the lock preserves routing
behavior; the ledger is needed only for audit or later optimization.

The generic Eval Exchange and low-level policy tools remain available. An
operator can `eval snapshot freeze`, `policy compile`, `policy diff`, and
`policy publish` for migration or a separately managed workflow. Low-level
`publish` promotes the exact candidate produced by `compile`; its embedded
parent digest is the compare-and-swap token, so stale and concurrent publishers
cannot overwrite a newer lock. These commands are not approval stages for
`optimize run`. Frozen mode rejects low-level/direct publication without
changing the active file; invoking `optimize run` explicitly authorizes
activation of adaptive mode and autonomous publication of its successor.

Eval storage is ownership-scoped without adding tenant fields to the wire
contract: local CLI records belong to `local`, while authenticated REST records
belong to the virtual key's user. A frozen snapshot commits both subject and
result digests. Multi-decision episodes must use `decision_credit.metric_ids`
to attribute quality, cost, latency, and violations; full implicit credit is
allowed only for a single decision.
