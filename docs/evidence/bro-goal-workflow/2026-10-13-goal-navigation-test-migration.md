# Goal navigation and retained native history

The new Goal entry does not own a foreground native controller. Navigating or
detaching therefore cannot inherit the old Code exit behavior of stopping that
controller. The retained native run inspector remains a separate read-only
alternate-screen interface.

The replacements preserve the meaningful history, isolation and terminal
contracts while exercising the supported entries:

| Retired product case | Current replacement |
| --- | --- |
| `code_detached_backlog_catches_up_once_after_resizes` | `goal_directory_resize_defers_backlog_and_isolates_native_inspection` commits 90 identified Goal history records while observing the directory and checks each is emitted once after returning to the Goal. `native_attached_backlog_preserves_all_events_across_resize_and_reconnect` receives 90 actual ACP updates, verifies complete ordered retained events, displays the last update after resizing and reconnecting, and confirms one original prompt. |
| `background_history_is_only_rendered_in_explicit_inspector` | The Goal history test launches a separate actual ACP background run with private output, views it only through explicit `agents attach`, and checks that its output never enters the Goal terminal. Both UIs restore the terminal and detach without stopping or claiming that run. The Goal draft survives. |
| `code_agents_navigation_keeps_normal_buffer_and_defers_foreground_output` | The Goal history test repeatedly visits the directory, suspends/resumes there, searches and resizes, then returns with the exact draft and no premature history. Normal navigation does not use an alternate screen or clear scrollback; directory controls do not enter retained scrollback. The active Goal PTY test separately covers real parallel executions and pending permissions arriving while reading a branch. |

The 90 Goal records are genuine persistent `SetBudget` receipts, preserving
the existing limits. That Goal has an elapsed limit of zero and is intentionally
cold: these records prove UI history/catch-up, not active worker output or model
quality. Native updates are produced by a Cargo-built Rust ACP fixture using
the real supervisor, retained history and read-only inspector. Fixed provider
and typed responses in the active Goal PTY test are also fixture evidence.

## Cache regression

The view previously rebuilt its transcript from current state when terminal
dimensions changed, including while browsing Goals, Workflow or Detail. A
deterministic cache test with new events already present in state reproduced
premature display on width change. Keeping logical displayed lines lets a
resize rewrap those same lines; returning to the Goal refreshes the transcript.
Scope changes still reset the cache through the existing document boundary.

The first red run failed on the deferred-content assertion: run
`2e622a40-1b95-4de4-807f-868a9f48dcee`, one failed. The first green attempt fixed
the behavior but its assertion incorrectly assumed wrapping preserved spaces
between rows. After correcting that assertion, all 24 Goal state/cache tests
passed, run `14fac97f-19c8-4fb8-be6f-a3474c8c1a09`, 0.036 seconds. Final source
checks are recorded in the implementation ledger.

Before this cache change, the Goal history PTY v3 passed (run
`3bdf055d-9e56-4e86-8954-5e4b4f9421bb`, 86.616 seconds). It did not reproduce
the cache defect, so it is not cited as a failing UI reproduction. Its first
attempt failed on an observer authorization mismatch; the test now requests
the existing Peek scope instead of broadening the production protocol. A
second attempt did not compile because authorization requires a set.

The native backlog test's first build rejected optional event cursors. Using
the existing integer cursor zero, the second run passed: run
`7ed3f6fc-3d2f-4053-8d3f-f600c6bea839`, 82.425 seconds, no leak report.

The first combined navigation build was stopped before test execution to fix
a test expectation for the actual search label (`Search this page / workflow`).
Final combined verification and retirement status remain in the ledger.

Combined v2 ran all five cases: four passed and the history case reached its
final scrollback inspection but failed inside vt100 0.15.2. That library
subtracts the requested scrollback offset from the drawing-row count; requesting
2,000 rows on a 24-row parser underflows. The test now temporarily enlarges only
its already-held parser grid, reads all retained rows, then restores its parser
dimensions. It does not resize the actual PTY or weaken the retained-history
assertion. The failed log remains; v3 reruns the complete five-case set.

Combined v3 passed all five actual PTY cases before the three retired cases
and their unused entry constructor/readiness helper were removed. Run
`4cf550e2-8399-46a8-8404-7166796aa633`, exit 0, 145.495 seconds. The source
manifest is `2026-10-13-goal-navigation-migration-v3-source.sha256`. Final
workspace verification runs against the separate final-v1 source manifest.

Full workspace final-v2 failed before running tests: the linker reported
`write() failed, errno=28` while building `managed_native`; only 199 MiB remained.
All owned Cargo/rustc processes had exited. The target and deps paths were
verified as actual directories under this worktree and ignored by Git.
`cargo clean --package bitrouter --target-dir` was first inspected with a dry
run: 2,887 regenerable package artifact files, approximately 21 GiB, all in this
worktree's target. Only those artifacts were then cleaned. Source, evidence,
other worktrees and dependency caches were preserved. The same source/debug
settings and test deadlines are used for full workspace final-v3.
The actual clean removed 2,856 files, 20.1 GiB; free disk space returned to
approximately 20 GiB. All 98 source hashes still matched after cleanup.

Full workspace final-v3 finished: 4,553 tests, 4,552 passed, one failed,
22 skipped by the existing configuration, exit 100, 979.900 seconds. Run
`0cce8f40-5570-44f4-9b4e-2ca1f19237d7`; no leak or timeout result. The sole
failure was the old onboarding status expectation for an ACP hint, while the
S6 production hint already opens Goals. The test expectation now checks Goals,
credential signals and structured output; its serialization uses Result.

Only `apps/bitrouter/src/onboarding.rs` changed between the final-v1 and
2026-10-14 final-v2 manifests, within that test. No production behavior changed
in this correction. The same workspace feature graph reran all onboarding
unit tests and Goal state/cache tests: 38 passed, exit 0, 0.146 seconds, run
`477c3d4c-116d-44da-bd72-08a774ab35f0`. This is a targeted repair verification,
not a second full-suite result. Final static/doc checks remain in the ledger.

Final-v2 workspace/all-target/all-feature Clippy with warnings denied passed,
76 seconds. Separate workspace doctests passed: six passed, one existing
ignored example, seven suites. Formatting, diff and all 98 source hashes passed.
The startup probe again printed its main-entry marker and exited successfully.
No owned Cargo, Rust compiler, terminal test or ACP fixture process remained.
Input quality, resource adaptation, real provider/harness acceptance and final
MVP gates remain incomplete.
