---
type: changed
breaking: true
title: "`PolicyKeyStrategy` exposes only the canonical `AgentTrace` variant"
pr: 828
---

`PolicyKeyStrategy` now exposes only `AgentTrace`. The `WorkflowState`
compatibility variant is gone — replace `PolicyKeyStrategy::WorkflowState` with
`PolicyKeyStrategy::AgentTrace`, and the `workflow_state` and
`legacy_fingerprint` config spellings with `agent_trace`.

The decision types keep their existing Rust field names: `PolicyDecision` keeps
`workflow_state_kind` and `workflow_identity`; `PolicyDecisionRecord` keeps
`workflow_state` and `workflow_identity`; and `PolicyDecisionSummary` keeps
`by_workflow_state`. Their JSON output uses canonical `trace_state`,
`trace_identity`, and `by_trace_state` names while accepting the old JSON
spellings on input. The matching `trace_*` accessors are available for new Rust
callers.
