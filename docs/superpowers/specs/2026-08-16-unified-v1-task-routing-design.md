# Unified v1 Task-Aware Routing Design

## Goal

Replace the task-aware router's dual `agent_route/v1` and `agent_route/v2`
protocol with one intentionally breaking `agent_route/v1` protocol. The result
must expose one canonical route-key shape, one fallback rule, one current
predictor contract, and one clean policy-lock representation.

## Canonical route contract

The only predictive policy key is:

```text
agent_route/v1|<task-family>|<role>|<risk>
```

`<task-family>` is one of the twelve classified Code/Agent families or
`unknown`. `<role>` and `<risk>` retain their existing closed enumerations.

The following inputs are invalid and must be rejected by parsers and policy
validation:

- the former three-segment `agent_route/v1|<role>|<risk>` key;
- every `agent_route/v2|...` key;
- unrecognized task-family, role, or risk literals;
- extra or missing segments.

`agent_trace/v2` remains an independent telemetry projection. It is not a
predictive policy version and is no longer a static-policy fallback source.

## Projection types and interfaces

`PredictiveRouteProjection` becomes the sole predictive projection type and
contains `task_family`, `next_step_role`, and `risk`. The separate
`TaskAwarePredictiveRouteProjection` type, v1 compatibility projection,
compatibility accessors, and compatibility parser branches are removed.

Online workflow state always emits the four-segment v1 key. Low-confidence or
unclassified requests use the `unknown` task family rather than reverting to a
different key schema.

Durable decisions retain two general-purpose facts:

- `route_projection`: the task-specific canonical key that was evaluated;
- `request_key`: the exact policy entry that matched.

The compatibility-specific `predictive_v1_fallback_tier` field is removed from
policy decisions, eval subjects, settlement records, trajectory evidence, and
optimizer observations. Existing static/baseline tier fields carry the tier
that actually governed the decision.

## Lookup and fallback

Static policy lookup follows exactly this order:

1. the exact `agent_route/v1|<task-family>|<role>|<risk>` entry;
2. `agent_route/v1|unknown|<role>|<risk>`;
3. the policy's configured default tier.

Observed `agent_trace` keys, the former three-segment predictive key, and
legacy fingerprints do not participate in named auto-router lookup.

The shipped template defines the complete fifteen-cell `unknown` baseline for
five roles and three risks, plus sparse task-family overrides. This preserves a
compact configuration without introducing a second defaults schema or a full
task-family Cartesian matrix.

## Predictor and policy locks

The deterministic predictor contract remains version 1. A predictive policy
lock is valid only when its descriptor exactly matches the current compiled
contract. The prior-scorecard digest allowlist and v1-only migration exception
are deleted.

Policy validation accepts canonical four-segment v1 predictive keys and the
unrelated supported policy namespaces only where still part of the current
product contract. It reports a direct validation error for retired predictive
key shapes. No automatic migration is provided.

Certificates, compiler evidence, settlement attribution, and optimization are
grouped by the primary canonical four-segment v1 route. When an exact task cell
uses the `unknown` baseline, `request_key` records that matched baseline and the
existing baseline/static tier evidence records its tier.

## Template and documentation

The auto-router policy lock, metadata, README, and adaptive-routing reference
use only the unified v1 terminology. They describe exact-to-unknown fallback
and contain no task-aware v2 or legacy-v1 migration instructions. Canonical
digests and certificates are regenerated through repository-owned code.

## Verification

Test-driven implementation must prove:

- the new four-segment v1 parser and key generation;
- rejection of old three-segment v1 and all agent-route v2 keys;
- exact task override, unknown-family baseline, and policy-default resolution;
- no observed-route fallback in named auto routing;
- exact current predictor-contract admission and rejection of the prior digest;
- durable/eval/trajectory/optimizer attribution without compatibility fields;
- equivalent Codex, Claude, Terminus 2, and generic HTTP routing;
- template key set, certificates, metadata, and config validation;
- full all-feature tests, strict Clippy, formatting, and diff checks.

Repository searches must find no production `agent_route/v2`,
`TaskAwarePredictiveRouteProjection`, `predictive_v1_fallback_tier`, or
predictive-v1 compatibility accessor after the refactor.
