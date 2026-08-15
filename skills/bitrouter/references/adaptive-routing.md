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
`bitrouter optimize setup`.

The same policy is also reachable through the generic `@preset[:variant]` form
(`@auto`, `@auto:cost`) that every preset uses; `bitrouter/auto` is the
documented spelling. Explicit physical model ids remain passthrough. Copy the template, then start
the daemon with its config and validate it before serving traffic:

```bash
bitrouter config validate --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
```

The starter runs with `policy.mode: frozen`. That process mode keeps routing
deterministic and forbids active lock replacement while telemetry and reward
evidence continue to accumulate. Change the main config to
`policy.mode: adaptive` for an evolution run. The lock itself never selects the
runtime mode.

## Route inputs and safety

The lock uses `key_strategy: agent_trace`. It routes the generic normal
`edit`, `test`, and `tool_followup` projections to economy; guarded or
unmatched projections use strong. Keep `tool_use_tier` and `tool_safe_tiers`
in place when changing tier models, so capability guardrails still apply.

Runtime adapters may parse native request formats to enrich diagnostic
evidence. They do not contribute policy keys, and no private BitRouter headers
are required for routing. Existing `workflow_state` lock configuration remains
readable as a compatibility alias, but deterministic lock output is canonical
`agent_trace`. `agent_trace` is the only active strategy:
`key_strategy: legacy_fingerprint` is rejected, so migrate its routes to
`agent_trace/v2|<state>|<risk>`, where risk is `normal`, `context`, or
`guarded`. Published v1 routes remain compatible through exact fallback.
`adequacy.explore_opening: true` opts
source-neutral opening projections into exploration. Do not configure the
removed `adequacy.max_downgraded_requests_per_session`; session identity is
diagnostic-only and the parser rejects that setting.

## Generic evaluation and publication

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

The hot path never reads eval rows. Freeze admitted evidence, compile a
candidate, and inspect the diff:

```bash
bitrouter eval status --config bitrouter.yaml
bitrouter eval result submit result.json --config bitrouter.yaml
bitrouter eval snapshot freeze --config bitrouter.yaml
bitrouter policy compile --eval-snapshot sha256:... --output candidate.yaml --config bitrouter.yaml
bitrouter policy diff policy-lock.yaml candidate.yaml
bitrouter policy check --config bitrouter.yaml
bitrouter policy publish candidate.yaml --config bitrouter.yaml
```

Only an explicit publication under `policy.mode: adaptive` may replace the
active lock. `frozen` and `adaptive` route identically; mode controls write
authority, not request-time learning. `bitrouter policy verify --evidence`
reconstructs an active compiled lock's evidence root when the local ledger is
available. Shipping only the lock preserves routing behavior; the ledger is
needed only for audit or later compilation.

`publish` promotes the exact candidate produced by `compile`. Its embedded
parent digest is the compare-and-swap token, so stale and concurrent publishers
cannot overwrite a newer lock. A frozen process rejects publication without
changing the active file.

Eval storage is ownership-scoped without adding tenant fields to the wire
contract: local CLI records belong to `local`, while authenticated REST records
belong to the virtual key's user. A frozen snapshot commits both subject and
result digests. Multi-decision episodes must use `decision_credit.metric_ids`
to attribute quality, cost, latency, and violations; full implicit credit is
allowed only for a single decision.
