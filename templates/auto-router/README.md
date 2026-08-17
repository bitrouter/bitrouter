# Predictive `bitrouter/auto:cost` routing

This starter policy predicts the task family and role of the next response from
the native prompt and causal history. It first looks for a sparse exact
`agent_route/v1|<task-family>|<role>|<risk>` override; when that cell is not
listed, it falls back to the complete
`agent_route/v1|unknown|<role>|<risk>` baseline. It uses GPT-5.6 as the strong
route, Kimi K3 as the balanced route, and DeepSeek V4 Pro as the economy route.

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

Use `bitrouter/auto` for the strong base policy, or `bitrouter/auto:cost` to add
the top-level cost routing variant. The generic `@auto` / `@auto:cost` preset
form addresses the same policy. Physical model ids remain passthrough, so an
explicit `openai-codex:gpt-5.6-sol` request is not converted to a preset.

The lock publishes all fifteen
`agent_route/v1|unknown|<role>|<risk>` baseline cells. The predicted roles are
`orchestrate`, `implement`, `mechanical`, `verify`, and `finalize`; the risk
bands are `normal`, `context`, and `guarded`.

When a task family is confidently classified, its primary key is
`agent_route/v1|<task-family>|<role>|<risk>`. The twelve task families are
`code:generation`, `code:debugging`, `code:review`, `code:sql_database`,
`code:frontend_ui`, `code:devops_config`, `code:repository_analysis`,
`agent:multi_step_planning`, `agent:workflow_execution`, `agent:web_research`,
`agent:memory_operations`, and `agent:general`. The template deliberately
publishes only three task-specific experiments:

- `agent_route/v1|code:review|verify|normal` → `strong`
- `agent_route/v1|code:debugging|implement|guarded` → `strong`
- `agent_route/v1|agent:web_research|mechanical|normal` → `balanced`

Those are exact overrides, not a full matrix: every other classified task cell
falls back to the v1 `unknown` task-family baseline for the same role and risk.
Shell dispatch, file dispatch, and tool dispatch stay bounded action/role
evidence; they do not create task-family cells or change a task-family route.
Runtime adapters may enrich diagnostics from native request shapes, but headers
and harness identity do not choose a tier.

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

The example treats both `strong` and `balanced` as progress-capable protected
tiers while retaining `strong` as the structural escalation tier. Ordinary
balanced work therefore remains balanced through progress accounting instead
of being promoted solely because it is not strong. Incomplete history, a
recovery edge, or a configured structural bound activates the guard: an already
protected candidate stays at its tier, while an unprotected candidate escalates
to `strong`. The guard then holds a protected tier for the next two requests.
`max_recovery_count` is edge-triggered: its prospective
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

The fifteen unknown-family baseline routes and three sparse task routes are compiler-owned
experiments with one matching lockfile certificate per explicit route.
Certificate evidence digests bind the policy name, predictive key, and selected
tier; the shared compiler digest binds the complete template policy.
Task-specific cells stay experimental until settled evaluation evidence promotes
them. Cross-agent quality has not been validated; evaluate the policy against
your own traffic before broad rollout.
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
