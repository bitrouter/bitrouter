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
`bro policy init`; it does not fall back to a provider default.

The template binds a named router; `bitrouter/auto` is its public spelling. Explicit physical model ids remain passthrough. Copy the template, then start
the daemon with its config and validate it before serving traffic:

```bash
bro config validate --config bitrouter.yaml
bro serve --config bitrouter.yaml
```

The router form of `policy init` preserves `policy.mode`; a missing mode remains
`frozen`. Routing stays deterministic from the signed lock: Eval rows never
mutate live routes on their own. Set the process mode to `adaptive` only when an
explicit optimizer or low-level publication command may replace the active
lock. The legacy `policy init --preset` form retains its historical behavior of
writing `policy.mode: adaptive`. The lock itself never selects runtime mode.

## Route inputs and safety

The starter is a version 4 named policy with a strong default and no pretrained
routes. Every tier is `{model, effort?, context}`; context must be `preserve` or
`evidence`. A supplied effort is policy-owned; omission preserves caller effort.

Request history and Core task/evidence signals enter one bounded System One
classifier. Rich API history can supply workflow state. Accepted task family and
next-step role yield `semantic_route/v1|<task-family>|<role>|<risk>`; exact cells
precede the unknown-family baseline and then the default. Truncated context and
observed recovery affect risk. Missing or low-confidence labels abstain.
Private headers and harness names do not select an algorithm or tier.

Configure `decision_model.model` and its credential environment variable to
activate the built-in TypeSafe protocol adapter. SDK hosts may inject another
`DecisionExecutor` backend, including Jev with an adapter. A different base URL
alone does not translate protocols. Backend configuration changes need restart.

`context: evidence` allows the shared planner to select among a trusted owner's
views. It does not grant omission, extract, summary or recall authority. Ordinary
HTTP prompts retain their source history unless a trusted host supplies those
capabilities and alternatives. Keep `tool_use_tier` and `tool_safe_tiers` aligned
with your model actions.

Old lock versions, scalar tiers, global `policy_table`, and the independent
`decision_model.policy.generation_models` list are rejected. Write a current
lock instead of carrying old scorecard evidence into System One routes.

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
bro policy init auto --preset auto --economy provider:model
# run a coding agent or Terminal Bench normally through bitrouter/auto
bro eval result submit result.json --config bitrouter.yaml
bro optimize run --policy auto --config bitrouter.yaml
bro optimize status --policy auto --config bitrouter.yaml
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
mode controls write authority, not request-time learning. `bro policy
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
