# Adaptive routing

Use the `templates/auto-router` starter when you want a conservative adaptive
policy without coupling it to an agent or workflow. Its config exposes the
standard `@preset[:variant]` form:

```text
@auto       # strong base policy
@auto:cost  # same policy plus the top-level cost variant
```

Explicit physical model ids remain passthrough. Copy the template, then start
the daemon with its config and validate it before serving traffic:

```bash
bitrouter config validate --config bitrouter.yaml
bitrouter serve --config bitrouter.yaml
```

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
`agent_trace/v1|<state>|<risk>`. `adequacy.explore_opening: true` opts
source-neutral opening projections into exploration. Do not configure the
removed `adequacy.max_downgraded_requests_per_session`; session identity is
diagnostic-only and the parser rejects that setting.

## Evaluation and publication

Treat the starter as a baseline. Evaluate it against representative traffic,
then review generated candidates before publication. A frozen lock is
deterministic and can be checked or reloaded without permitting programmatic
replacement:

```bash
bitrouter policy check --config bitrouter.yaml
bitrouter policy reload --config bitrouter.yaml
```
