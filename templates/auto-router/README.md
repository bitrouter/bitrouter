# Predictive `@auto:cost` routing

This starter policy predicts the role of the next response from the native
prompt and causal history, then selects a tier from the resulting
`agent_route/v1|<role>|<risk>` key. It uses GPT-5.6 as the strong route, Kimi K3
as the balanced route, and DeepSeek V4 Pro as the economy route.

Tier values are model targets. The scalar form above remains compatible and
preserves a caller-supplied reasoning effort. For a model that positively
declares exact effort support in the registry, a tier may instead own effort:

```yaml
tiers:
  strong: { model: "openai-codex:gpt-5.6-sol", effort: high }
  economy: { model: "openai-codex:gpt-5.6-sol", effort: low }
```

Those are distinct routing targets even though they share a model. Predictor,
tool guard, progress guard, continuation, evidence, and fallback behavior stay
the same; the selected effort is applied by the BitRouter daemon and rendered
in the upstream protocol's native field.

The frozen mapping is intentionally aggressive: normal mechanical and verify
work uses economy; implementation, finalization, and context-heavy work mostly
use balanced; orchestration and selected guarded work use strong. Unknown or
unmatched predictions use the balanced default. Prediction is deterministic and
does not add a controller call or modify the request.

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

The v3 lock uses generic `agent_route/v1|<role>|<risk>` keys. The predicted roles
are `orchestrate`, `implement`, `mechanical`, `verify`, and `finalize`; the risk
bands are `normal`, `context`, and `guarded`. Runtime adapters may enrich
diagnostics from native request shapes, but headers and harness identity do not
choose a tier. Existing `agent_trace/v2` and `agent_trace/v1` locks remain
routable through exact compatibility fallbacks.

All three tiers are listed in `tool_safe_tiers`. Supplying tools therefore does
not clamp a predictive selection to another tier, and the template never
removes, renames, constrains, or rewrites tool definitions. `tool_use_tier`
remains `strong` as the fallback for a future tier that is not declared
tool-safe; it does not override any tier in this template.

Prediction and observation are separate. The decision record contains the
predicted role and action before generation; the response observer later records
only a bounded categorical action. If the strong model mutates when a different
action was predicted, analysis counts that as an action mismatch. The mismatch
is descriptive evidence only: it is not a quality verdict, does not rewrite the
response, and does not mutate the active route during the request.

This template explicitly enables local trajectory persistence and carries a
conservative `progress_guard` example in its signed policy. These settings are
template policy, not hidden runtime defaults: trajectory remains disabled by
default for existing configurations, and existing locks are unchanged. All
three `trajectory` settings are restart-only.

The example immediately selects `strong` for incomplete history, a recovery
edge, or a configured structural bound, then holds that protected tier for the
next two requests. `max_recovery_count` is edge-triggered: its prospective
cumulative count is compared only when the current projection enters
`recovery`; consecutive recovery projections remain protected without counting
or activating again. The edge always activates hold. If the recovery's static
candidate is any declared protected tier, that exact tier is preserved for the
current request; otherwise the request selects `strong`. An active hold follows
the same non-downgrade rule without resetting its duration.
After the hold expires, an ordinary projection can return to its static route
unless another clause fires. Episode request and elapsed-time limits are
monotonic once reached; consecutive and same-projection limits bound repeated
unprotected routing. The example omits a cost limit because unknown cost must
remain unknown rather than act like zero or trigger a fabricated threshold.

Raw prompt text, response text, and tool arguments are not persisted as routing
evidence. The ledger retains structural ancestry, categorical projections and
risk, exact counters, and keyed digests.

The fifteen starter routes are compiler-owned experiments with one matching v2
certificate per route. Certificate evidence digests bind the policy name,
predictive key, and selected tier; the shared compiler digest binds the complete
template policy. Cross-agent quality has not been validated; evaluate the policy
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
