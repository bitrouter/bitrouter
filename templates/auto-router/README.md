# Adaptive `@auto` routing

This starter policy uses GPT-5.6 as the strong route, Kimi K3 as the balanced
route, and DeepSeek V4 Pro as the economy route. It is agent- and
workflow-independent: ordinary `edit`, `test`, and `tool_followup` projections
use economy; read-only `review` and long-context execution projections use
balanced; guarded and unmatched projections use strong.

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

The v2 lock uses generic `agent_trace/v2|<state>|<risk>` keys. The `context`
risk band is separate from hard `guarded` recovery/redo/precision risk. Runtime
adapters enrich diagnostics from native request shapes but do not supply policy
keys. No private BitRouter headers are required. Published `agent_trace/v1`
locks remain routable through an exact compatibility fallback; new default
decisions and learning evidence use v2.

The v2 request projection also bounds model-induced loops without keeping a
source-specific session budget. A visible agent trajectory with eight prior
assistant action turns raises expected redo risk to `guarded`, and an observed
execution failure remains guarded through the next two execution observations.
Both signals are reconstructed from inbound message history. Ordinary long
conversations without agent actions are unaffected; protocols that hide prior
turns retain explicit visibility-gap evidence instead of inventing state.

The eight starter routes are compiler-owned experiments informed by settled
trace analysis, model protocol canaries, and synthetic long-context action
qualification. Cross-agent quality has not been validated; evaluate the policy
against your own traffic before broad rollout.
Admitted evidence can promote or demote compiler-owned routes. Routes explicitly
authored as operator-owned remain pinned: conflicting evidence is reported
rather than overriding the operator's route.

Before a live experiment, estimate the maximum useful surface from settled
baseline traffic. The effective cost factor includes expected token, retry, and
turn inflation, not just provider list price:

```bash
bitrouter workflow-state policy-oracle \
  --traces traces.jsonl \
  --cloud-usage usage.jsonl \
  --policy-lock policy-lock.yaml \
  --policy auto \
  --effective-cost-factor 0.24 \
  --target-savings 0.30 \
  --target-savings 0.40 \
  --output oracle.json
```

The oracle is a cost-only upper-bound replay. It ranks eligible requests and
routes by baseline cost, and separately surfaces the costly routes still left
on the default tier. It does not assume the remainder of a live agent trajectory
will stay unchanged.

The daemon creates redacted request subjects automatically. External evaluators
submit results through `bitrouter eval result submit` or `POST /v1/evals/results`.
Freeze admitted evidence and compile without changing the active lock:

```bash
bitrouter eval snapshot freeze --config bitrouter.yaml
bitrouter policy compile --eval-snapshot sha256:... --output candidate.yaml --config bitrouter.yaml
bitrouter policy diff policy-lock.yaml candidate.yaml
bitrouter policy publish candidate.yaml --config bitrouter.yaml
```

`publish` requires `policy.mode: adaptive` and rejects stale parent digests.
The runtime's `policy.mode`, rather than lockfile contents, owns publication
authority: frozen mode never replaces the active lock, while adaptive mode only
permits this explicit publish step.
After explicit publication, `bitrouter policy verify --evidence --config
bitrouter.yaml` reconstructs the active lock's evidence root from the local
ledger.
