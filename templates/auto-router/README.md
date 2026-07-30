# Adaptive `@auto` routing

This starter policy uses GPT-5.6 as the strong route and DeepSeek V4 Pro as
the economy route. It is agent- and workflow-independent: ordinary `edit`,
`test`, and `tool_followup` projections use economy; guarded and unmatched
projections use strong.

Start BitRouter from this directory:

```bash
bitrouter serve --config bitrouter.yaml
```

The template starts in `policy.mode: frozen`: routes are deterministic, ledger
state cannot change live decisions, and BitRouter will not replace the active
lock. Traces, metering, eval subjects/results, snapshots, and separate candidate
exports remain available. Set `policy.mode: adaptive` only when the process may
publish a reviewed candidate; it does not enable request-time learning.

Use `@auto` for the strong base policy, or `@auto:cost` to add the top-level
cost routing variant. Physical model ids remain passthrough, so an explicit
`openai-codex:gpt-5.6-sol` request is not converted to a preset.

The v2 lock uses the generic `agent_trace` key strategy. It contains effective
routing plus a certificate for every explicit route, but no activation/freeze switch. Runtime adapters can
enrich diagnostics from native request shapes, but do not supply policy keys.
No private BitRouter headers are required. The strong/economy tool capability
guardrails remain in the lock, so a request only uses economy when its route
and capability constraints permit it.

This policy was migrated from a same-scenario evaluation. Cross-agent quality
has not been validated; evaluate it against your own traffic before broadening
the economy routes.

The daemon creates redacted request subjects automatically. External evaluators
submit results through `bitrouter eval result submit` or `POST /v1/evals/results`.
Freeze admitted evidence and compile without changing the active lock:

```bash
bitrouter eval snapshot freeze --config bitrouter.yaml
bitrouter policy compile --eval-snapshot sha256:... --output candidate.yaml --config bitrouter.yaml
bitrouter policy diff policy-lock.yaml candidate.yaml
```

After explicit publication, `bitrouter policy verify --evidence --config
bitrouter.yaml` reconstructs the active lock's evidence root from the local
ledger.
