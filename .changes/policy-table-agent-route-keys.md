---
type: changed
breaking: true
title: "Active `policy_table` routing uses the `agent_route/v1` predictive contract"
pr: 828
---

Active `policy_table` routing now uses one predictive route contract:
`agent_route/v1|<task-family>|<role>|<risk>`. Exact task-family routes fall back
to the corresponding `unknown`-family role/risk baseline, then to the policy
default. Observed `agent_trace/v2` keys remain telemetry only.

Static `agent_trace` routes, three-segment predictive v1 routes, and all v2
predictive routes are rejected during config and lock validation. Regenerate
policy locks and certificates with the current predictor contract.

`key_strategy: agent_trace` selects this deterministic predictor; the retired
`workflow_state` and `legacy_fingerprint` spellings are rejected.
`adequacy.max_downgraded_requests_per_session` is rejected: session identity is
diagnostic-only and no longer affects routing.
`adequacy.explore_opening` is honored for source-neutral opening projections.
