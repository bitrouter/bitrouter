# BRO Conversation UI integration evidence

Date: 2026-10-04. Contract: [BRO_CONVERSATION_UI_SPEC.md](BRO_CONVERSATION_UI_SPEC.md).
Validated source: `9e09ce40` on macOS with Rust 1.97.0. This ledger is a
documentation-only follow-up to that source. Builds used incremental=0 and
dev/test debug=0. Hosted CI for the published head is a separate gate.

## Delivered behavior

- PR #952 (`281619ff`) is an ancestor of this integration, including its shared
  ACP navigation and PTY behavior. PR #945's subsequent six-tool integration
  (`348106a7`) is retained. Explicit `bro code <agent>` remains ACP.
- Bare `bro code` uses the normal-buffer Writer, chronological committed Items,
  Markdown, a replaceable live tail, and the shared grapheme-aware composer.
  A separate model editor preserves the conversation draft.
- Plain Left with an eligible empty composer opens a durable BRO Thread
  directory. Search/filter/preview are local presentation; `o` explicitly
  restores and observes a selected Thread without executing it.
- Cold `ListThreads` reads are caller/workspace/profile filtered, paginated at
  a membership cutoff, bounded to 16 roots and existing store byte limits, and
  install no context, worker, subscriber or runner. Resident pages refresh
  current public state every two seconds. Local protocol is v15; HTTP v2 is
  unchanged and has no new directory endpoint.
- History reconstruction uses one public durable cutoff and stable entity IDs.
  Observation resynchronization restores that same snapshot cutoff; stale live
  output cannot overwrite a newer durable projection. Accepted steering status
  is projected into the transcript using its stable input ID.
- Enter/enqueue, Ctrl-Enter/steer, Ctrl-R/resume, identified approval/cancel,
  uncertain acceptance and detach keep their ThreadService ownership. Menu
  events do not reach composer/approval input; an arriving approval remains
  server-owned without changing menu focus. Switching is blocked by unresolved
  acceptance or a nonempty draft. Below 40×16, submission/approval are disabled.
- Resize rewraps the frozen transcript; suspend/resume restores raw mode,
  bracketed paste and the input stream while retaining state. Opening another
  Thread begins below the prior document in native terminal history.
- CLI, README, architecture, skill and all three plugin manifests describe the
  delivered entry points and protocol. The skill entry remains under 200 lines.

## Local validation

| Gate | Result |
| --- | --- |
| `cargo +1.97.0 nextest run --workspace --all-features` | **3,712 passed, 22 skipped**; no reported leaks. Run `f4b9989f-97b8-4129-bcb0-75ddeaad2bc3`. |
| `cargo +1.97.0 test --doc --workspace --all-features` | **5 passed, 1 ignored**. |
| `cargo +1.97.0 clippy --workspace --all-features --all-targets -- -D warnings` | Passed. |
| `cargo fmt --all -- --check`; `git diff --check` | Passed. |
| `RUSTDOCFLAGS="-D warnings" cargo +1.97.0 doc --workspace --all-features --no-deps` | Passed. |
| Plugin JSON; seven changed internal-document relative-link/fence checks; skill size | Passed. |

The complete suite includes these specific proofs:

- `directory_is_cold_paginated_and_caller_filtered`: unloaded roots are visible,
  foreign callers and revoked grants are filtered, page boundaries progress,
  oversize limits fail, and listing does not change residency, execution or the
  stored root version.
- `native_agents_navigation_opens_durable_history_without_submitting`: real
  local daemon, SQLite and PTY with a mocked upstream; browse/preview/open add
  zero model requests. A subsequent request includes the selected Thread's
  retained user/assistant context. Navigation survives 40×16 resize and actual
  SIGTSTP/SIGCONT. Raw output contains no alternate-screen or scrollback-clear
  sequence.
- `busy_enter_enqueues_and_control_enter_targets_active_turn`: a protocol
  fixture distinguishes enqueue from targeted steering; key release and narrow
  viewport cannot submit, and the active Turn ID remains unchanged.
- Existing native approval/reconnect/cancel PTY coverage retains an unsent
  draft, pending approval and durable queue through observation loss; repeated
  detach releases subscribers. Existing ACP navigation, composer, resize,
  external-editor and terminal-lifecycle tests also pass.

An early focused run reported a leak in the existing inspection-tools test.
Both subsequent complete suites passed without leaks, including the final
six-tool combination. Earlier suspend fixture failures were traced to accepting
a queued pre-suspend frame; the test now waits for post-resume bracketed-paste
enablement and a subsequent synchronized frame before typing.

## Unverified boundaries

This is local fixture evidence, not a new credentialed-provider or ACP run.
The UI integration has not been exercised on Windows/Linux in this session.
Search is confined to the current directory page; cutoff fixes membership,
not a global atomic snapshot of every row's status. Cold row projection can
scan bounded public history, and the client retains its restored transcript
in memory; long-history latency/memory stress is not established here.

Draft persistence across process exit, lost-owner/unknown-effect investigation,
operator recovery resolution, native multi-agent scheduling, inbound ACP and
orchestrator-core integration remain outside this UI change. Existing runtime
and six-tool evidence retain their own source/platform/provider boundaries.
