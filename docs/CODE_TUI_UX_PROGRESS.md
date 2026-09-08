# Code TUI implementation and acceptance evidence

Status: **implemented and locally verified**, 2026-09-08.

Implementation follows [CODE_TUI_UX_SPEC.md](CODE_TUI_UX_SPEC.md), including
the authorized conversation-first design and A1–A14 acceptance criteria.
The implementation and its validation evidence are prepared for pull-request review.

## Delivered behavior

- One conversation, multiline composer, and agent/route/activity/attributed-cost
  status replace the seven-page dashboard. Pickers and inspectors restore the
  draft and reading position; failed settings mutations restore their picker.
- Explicit FIFO follow-ups, retained tool/diff inspection, stable history
  anchors, Unicode editing, and external-editor recovery share one interactive
  driver across canonical and compatibility entries.
- ACP initial settings, native session IDs, lifecycle replay, and advertised
  controls drive the UI. Cancellation observes the original prompt result or
  bounded failure. Generic teardown denies abandoned permissions; explicit
  turn cancellation preserves the protocol's cancelled outcome.
- Remote and socket-only entries expose supported read-only operations with
  visible scope and errors. The presentation crate remains free of daemon and
  storage access; operational services own those dependencies.

## Final gates

Checks ran on macOS after rebasing the implementation onto `main` at
`6f10d528`, including the managed-update and Codex Responses fixes.

| Check | Result |
| --- | --- |
| `cargo nextest run --workspace --all-features --status-level fail` | 3,196 passed; 11 existing tests skipped |
| `cargo clippy --workspace --all-features --tests -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | Passed |
| `cargo test --doc --workspace --all-features` | 5 passed; 1 existing example ignored |
| Real-PTY acceptance suite, included in the full run | 22 passed; none ignored |
| Newly introduced repository-relative Markdown targets and `git diff --check` | Passed |
| New Rust panic/bypass patterns and ignored tests | None introduced |

The final full nextest run was `a7d031cf-d557-4fb9-83e5-ed4879f3bac4`.
The existing large-test-binary linker warning and `proc-macro-error2`
future-compatibility notice remain; neither caused a gate failure.

The real-PTY suite uses isolated homes, configuration, temporary directories,
and mock ACP agents without live credentials. It checks exact prompt bytes,
current terminal contents, `stty` restoration, and usable shell output after
exit. Waits are bounded by semantic conditions, including complete restored
picker content rather than the first chunk of a terminal redraw.

## Acceptance audit

Evidence locations:

- **UI:** [conversation state/render tests](../crates/bitrouter-tui/src/code.rs),
  [editor tests](../crates/bitrouter-tui/src/editor.rs), and retained journal,
  permission, command-resolution, and Markdown renderer tests in the same crate.
- **Driver:** [shared application loop](../apps/bitrouter/src/chat/code.rs),
  [wire lifecycle tests](../apps/bitrouter/src/chat/code_wire.rs),
  [session-host tests](../apps/bitrouter/src/acp_cli.rs), and the application
  dependency-boundary guard.
- **ACP:** [SDK client protocol tests](../crates/bitrouter-sdk/src/acp/client.rs).
- **PTY:** [real terminal journeys](../apps/bitrouter/tests/code_tui_pty.rs).
- **Output:** [ACP/headless/pipe integration tests](../apps/bitrouter/tests/acp.rs).

| Criterion | Result | Evidence |
| --- | --- | --- |
| A1 | Verified | PTY bare, explicit-agent, hidden `chat`, and hidden `tui` entries; operations bootstrap render test; old dashboard removed |
| A2 | Verified | UI modal/selector snapshots, async-inspector restoration, failed-mutation restoration, and streaming under inspection; driver consumes input and ACP during effects |
| A3 | Verified | UI 80×24/40×16 status/composer grids, unknown cost, provenance, large values, and activity transitions |
| A4 | Verified | 13 editor tests plus exact multiline PTY prompts/history, successful editor handoff, failed editor recovery, and terminal restoration |
| A5 | Verified | PTY FIFO, drafting, abnormal-stop and cancellation pause; UI completion-race pause, command revalidation, and long-queue editing |
| A6 | Verified | UI retained reading anchors across streaming, resize, and tool expansion; explicit return-to-live and bounded history rendering |
| A7 | Verified | UI explicit focus/highlight/confirmation and overlapping identities; PTY exact responses; wire tests distinguish explicit cancellation from teardown denial, including late requests |
| A8 | Verified | PTY cancellation and adapter failure; wire tests cover late updates, retained prompt lifetime, deadline ties, no queue release on disconnect, and deadlines under continuous updates |
| A9 | Verified | UI and command-resolution tests cover owner labels, explicit agent selection, legacy precedence, templates, stale commands, and exact fallback prompt bytes |
| A10 | Verified | Minimal-agent PTY conversation plus UI honest missing-capability/settings/cost states |
| A11 | Verified | PTY confirmed/failed settings and native load/resume; ACP replay/settings/list/route round trips; driver confirmed-result handling, initial settings seeding, and retained disconnected native identity |
| A12 | Verified | PTY authenticated remote reports, visible remote failure without local fallback, and socket-only read-only entry; service capability filtering and operations-only bootstrap |
| A13 | Verified | UI compact tools with full received output/diffs retained, generic inspection, sanitization, Unicode search, and source-labelled cost evidence |
| A14 | Verified | PTY canonical/compatibility paths, SIGINT, SIGTERM with exact permission denials, editor failure, adapter death, and shell restoration; existing plain-pipe/headless output and dependency guards |

The 80×24 and 40×16 status/composer grids, narrow permission grid, and resized
streaming-history grid were also inspected from the renderer's emitted test
output. All 28 conversation state/render tests passed in that review.

## Validation limits

[Native adapter smoke evidence](CODE_TUI_ADAPTER_SMOKE.md) records actual
versions and advertised capabilities for Codex ACP, Claude Agent ACP, and
OpenCode. These credential-free probes exercised `initialize` only, not
authenticated prompts or every advertised lifecycle operation.

Route confirmation/refusal is covered by the real controller/SDK round trip,
existing reducer checks, and driver source review. There is no dedicated Code
PTY route-mutation journey; the Code PTY suite directly exercises settings
confirmation/refusal and native lifecycle behavior. The local full-suite result
is not a claim that Linux or Windows CI has run.

CLI, development, historical-spec status notes, and the shippable BitRouter
skill/reference documents were updated in lockstep. The main skill remains
200 lines. Plugin invocation manifests still use the unchanged `mcp serve`
surface. The model/provider registry and product-docs repository were not
changed.
