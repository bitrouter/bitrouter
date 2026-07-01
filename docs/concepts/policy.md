---
title: Policy
description: The operator-owned spec that decides how a loop routes — deterministic, no LLM in the path, and off by default.
---

A **policy** is the spec that decides how a loop routes. It's operator-owned config, not a model: the routing decision is deterministic, adds no LLM call to the path, and every deployment ships with it **off by default**. It's the "act" surface of BitRouter's [observe → evaluate → act](/docs/get-started/introduction) loop — the file an agent (or you) edits to spend a capable model only where it's earned and a cheaper one everywhere else.

## The policy table

At its core a policy is a static, operator-owned **table** that picks the model per request instead of taking the caller's requested model at face value:

- **Fingerprint** the agent-loop step from the canonical prompt, by the model's most-recent turn — `opening`, `after_<tool>` (e.g. `after_read_file`), or `midstream`.
- **Resolve** fingerprint → tier → model id, and rewrite the request's model. An unmatched fingerprint falls back to `default_tier`.
- **Hard tool-use guardrail:** a request carrying tools is clamped up to a tool-safe tier, so a downgrade never strands a tool call on a model that can't handle it.
- **Idempotent, and it defers** to routes it doesn't own — an explicit `provider:` or `claude-code:` target, or the `bitrouter/fusion` alias, passes through untouched.

```yaml
policy_table:
  tiers: { cheap: openai:gpt-4o, capable: anthropic:claude-sonnet-4-6 }
  fingerprints: { after_read_file: cheap }
  default_tier: capable
  tool_use_tier: capable
  tool_safe_tiers: [capable]
```

That table alone is a complete, deterministic router. The rest of this page is the *adaptive* half — entirely opt-in.

## The adequacy ledger

Turn on `adequacy` and the router learns online, per request, without any round structure. An observer recomputes the fingerprint of each served request, maps the served model back to its tier, and — **only for a genuine downgrade** — records whether the request hard-failed:

- After `escalation_threshold` consecutive failures the fingerprint is **pinned** and escalated to a more capable tier. Pins persist locally and **decay after a cooldown**.
- With `explore_enabled`, the daemon periodically **trials** the cheap tier on fingerprints you left at the capable tier and **locks** the ones that keep succeeding — discovering safe downgrades automatically. A failed trial escalates and stops. The tool-use guardrail still clamps any trial of a tool request.

```yaml
  adequacy:
    enabled: true
    escalation_tier: capable
    escalation_threshold: 2
    pin_cooldown_secs: 1800
    explore_enabled: true     # the aggressive knob
    explore_tier: cheap
    explore_threshold: 3
    explore_interval: 5
```

## The guarantee

The rule is asymmetric in one direction: the learner can only ever make routing **more conservative** than your table. A downgrade that proves inadequate — one you configured *or* one the daemon discovered — is escalated and stays there; only a downgrade that keeps succeeding is kept. So a request never persistently routes to a worse tier, while cheaper routes are still pursued. Both halves are opt-in, so a deployment with `adequacy` off behaves byte-identically to the static table.

## Not the same as Cloud policy

This page is about **routing** policy in the local router. BitRouter Cloud has a *separate* policy surface — `bitrouter cloud policy` manages budgets, rate limits, guardrails, and presets bound to an API key or workspace. See the [CLI](/docs/concepts/cli) for those commands.

## Related

- [Provider selection](/docs/features/provider-selection) — how the providers behind a chosen model are ranked.
- [Model fallback](/docs/features/model-fallback) — walk an ordered list of models on failure.
- [Model routing](/docs/concepts/models) — why a model is an aggregate the policy routes across.
