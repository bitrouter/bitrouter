This draft recipe packages the deployable [`auto-router`](https://github.com/bitrouter/bitrouter/tree/main/templates/auto-router)
template without maintaining a second copy of its configuration or policy lock.
The catalog builder reads those artifacts directly, validates their provider,
model, preset, and lock references against the current registry, and embeds the
exact files in `dist/recipes/index.json`.

## How it routes

The policy consumes the generic `agent_trace/v2|<state>|<risk>` projection that
BitRouter derives from native OpenAI- and Anthropic-compatible request history.
It does not use benchmark IDs, task IDs, harness identity, or private routing
headers as decision keys.

- Guarded recovery, redo, precision, and unmatched states stay on the strong
  tier (GPT-5.6-sol).
- Read-only review and long-context execution states may use the balanced tier
  (Kimi K3).
- Ordinary edit, test, and tool-follow-up states may use the economy tier
  (DeepSeek V4 Pro).

The policy lock is the serving source of truth. `policy.mode: frozen` keeps the
route deterministic while still allowing observation and external evaluation.
Changing to `adaptive` only permits an explicit, reviewed candidate publish; it
does not enable request-time learning.

## Related mechanism evidence

PR [#768](https://github.com/bitrouter/bitrouter/pull/768) recorded two
independent accepted Terminal-Bench 2.1 short13 lineages, each with a fresh
policy database and a fixed GPT-5.6-sol control. Those lineages evaluated two
independently compiled R3 locks, not the current starter lock embedded by this
recipe:

| Lineage | Control | Frozen `@auto` R3 | Paired cost change |
| --- | --- | --- | ---: |
| 1 | 10/13, $5.070774 | 11/13, $4.102794 | -19.09% |
| 2 | 11/13, $6.105430 | 11/13, $5.233069 | -14.29% |

The combined measurements are 21/26 vs 22/26 passes and $11.176204 vs
$9.335863 total notional model cost: -16.5% aggregate cost and +3.8 accuracy
points. The mean of the two independently paired cost changes is -16.69%.

These numbers are deliberately not stored in `recipe.yaml`. The current starter
lock keeps eight routes as compiler-owned experiments, while each accepted R3
lock contains its own admitted-evidence lineage and a more conservative set of
strong routes. Treating the R3 result as a measurement of the starter artifact
would violate the recipe catalog's exact-artifact provenance rule.

One additional lineage was rejected and preserved after Kimi K3 returned a
storm of 503, 504, and 429 responses. It is not included in the effect numbers.
That failed attempt is important operational evidence, not a semantic quality
result, and motivates a separate availability plane.

## What this does not prove

The recipe therefore remains `draft` until this exact config and lock are run in
an accepted comparative evaluation. The existing runs are mechanism evidence,
not a public leaderboard result or a cross-workload guarantee. Their reported cost is a frozen notional
Codex subscription shadow series, kept separate from actual provider charges.
Across both accepted R3 runs, 169 of 171 requests still used GPT-5.6;
the observed saving primarily came from trajectory and token efficiency, not
aggressive weak-model substitution. Cross-agent quality remains unvalidated.

Evaluate the frozen policy against your own task-native outcomes before broad
rollout. Keep missing outcomes unknown, preserve rejected attempts, and publish
the recipe only after the catalog's config and policy-lock digests match the
accepted run inputs exactly.
