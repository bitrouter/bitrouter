---
type: changed
title: "Policy `workflow_*` Rust and JSON spellings stay readable as `trace_*`"
pr: 757
---

`PolicyKeyStrategy::WorkflowState` remains a compatibility variant and
serializes as `agent_trace`; use `PolicyKeyStrategy::AgentTrace` in new code.
`PolicyDecision` keeps `workflow_state_kind` and `workflow_identity`;
`PolicyDecisionRecord` keeps `workflow_state` and `workflow_identity`; and
`PolicyDecisionSummary` keeps `by_workflow_state`.

Their JSON output uses canonical `trace_state`, `trace_identity`, and
`by_trace_state` names while accepting the old JSON spellings on input. The
matching `trace_*` accessors are available for new Rust callers.
