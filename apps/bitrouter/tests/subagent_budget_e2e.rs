//! End-to-end budget-deny procedure for the `spawn_subagent` tool.
//!
//! This is the gating manual check from the spec (work items 7–8): prove the
//! cap actually fires and metering accrues for a RUNTIME-minted key against a
//! live daemon + a real provider that reports streaming usage.
//!
//! It is `#[ignore]`d because it needs `opencode` on PATH, a running daemon
//! (`examples/subagent-demo/bitrouter.demo.yaml`), and a real provider key — none
//! of which are available in unit-test CI. The body documents the procedure;
//! wire it against the running binary once a streaming-usage provider key is in
//! CI. Run explicitly with:
//!
//!   cargo test -p bitrouter --test subagent_budget_e2e -- --ignored --nocapture
//!
//! Manual procedure (do this before the demo):
//!   1. Fill `examples/subagent-demo/bitrouter.demo.yaml` `providers:` for a cheap
//!      model whose upstream reports STREAMING usage, including a pricing block.
//!   2. `bitrouter start --config examples/subagent-demo/bitrouter.demo.yaml`.
//!   3. Drive the parent agent to call `spawn_subagent(model, budget_micro_usd=1,
//!      task=...)` (a 1 µ$ cap).
//!   4. The cap is checked PRE-request against SETTLED spend (no per-request
//!      reservation), so the first inference runs; once its spend settles past the
//!      cap, the NEXT worker call is denied (`Forbidden`). Assert the structured
//!      tool result shows `capped: true` (and/or `stop_reason: "error"`).
//!   5. Cross-check `bitrouter cloud usage`: the worker key's spend == its cap.
//!   6. If spend stays 0, the upstream is not reporting streaming usage — switch
//!      models (spec §8). The cap silently no-ops on a `usage{0,0}` stream.

#[test]
#[ignore = "needs opencode + a running daemon + a real streaming-usage provider"]
fn cap_denies_worker_after_limit() {
    // Intentionally a no-op placeholder: this encodes the manual verification
    // procedure above. Implement it against the running binary (via
    // std::process / an HTTP client) once a streaming-usage provider key is
    // available in CI. Kept compiling + skipped so the procedure is discoverable
    // from the test list (`cargo test -- --ignored --list`).
}
