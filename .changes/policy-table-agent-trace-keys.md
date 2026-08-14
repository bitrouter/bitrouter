---
type: changed
breaking: true
title: "Active `policy_table` routing only accepts `key_strategy: agent_trace`"
pr: 757
---

Active `policy_table` routing now defaults to, and only accepts,
`key_strategy: agent_trace`. Replace legacy `key_strategy: legacy_fingerprint`
and `opening`/`after_*` routes with canonical `agent_trace/v1|<state>|<risk>`
routes. Historical policy locks spelling the strategy `workflow_state` remain
readable and serialize back as `agent_trace`.

`adequacy.max_downgraded_requests_per_session` is rejected: session identity is
diagnostic-only and no longer affects routing.
`adequacy.explore_opening` is honored for source-neutral opening projections.
