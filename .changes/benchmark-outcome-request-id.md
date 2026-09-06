---
type: changed
breaking: true
title: "`BenchmarkOutcomeRecord` carries a `request_id` for strict reward feedback"
pr: 757
---

`BenchmarkOutcomeRecord` has a new `request_id` field and strict reward
feedback joins it to the persisted `CapturedIngressTrace.id`. Migrate Rust
struct literals to `BenchmarkOutcomeRecord::new(session_key, task_id, reward)`
followed by `.with_request_id(trace_id)` when producing reward-feedback
artifacts.

Older outcome JSONL remains serde-compatible (`request_id` defaults to
absent), but it is analytical-only and strict feedback rejects it.
