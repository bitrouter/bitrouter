# Runtime evidence and planning contract

The planner now requires a controller acknowledgment only for a selected, supported model route. An uncontrolled harness default does not create an additional switch-ack prerequisite. Original Goal-wide orchestration requirements remain final completion conditions; workers cannot certify the controller's scheduling or other branches.

Input interpretation, handoff review and completion assessment receive public runtime observations bound to Goal, task version, execution, original start mapping, run, harness and canonical workspace. Sequence/cutoff mismatches become unavailable. The timeline is bounded to 64 selected observations, with explicit omission/completeness/order indicators. It excludes original prompt text, worker output, private reasoning, tool arguments and permission grants.

Prompt admission and Working state are not actual worker activity. A pending turn gets one activity marker from a public agent/tool update or incoming permission request; repeated output chunks coalesce, and late updates outside the turn do not add markers. A permission request supplies no consent or tool-completion proof. Timing cannot establish parallel work without complete ordered activity-to-settled boundaries.

## Checks

- v1: strict workspace/all-target/all-feature Clippy, five selected tests (two metadata tests and three real Cargo-built ACP fixture workflows), formatting, diff and 140 source hashes passed.
- v2: strict Clippy, six selected tests (three metadata tests and the same three ACP fixtures), formatting, diff and 140 hashes passed. Run `43104c41-9c61-4b44-8638-2d626168e5ac`, 127.510 seconds.
- Current production `bro` binary built successfully and was copied with verified SHA-256 `949b0215c81f62c4bab6c3a572d199806a09a8d662343e07de46b96a42ca885b` to the private test runtime. This source has not yet run the complete workspace suite; the previous 4,640-test full checkpoint covers the preceding 138-file source.

## Actual warm-profile attempt

Goal `3e666f6a-abd3-497c-a7ff-d878e12d1322` was created through actual `bro code` TUI paste/Enter after the TUI budget form saved 1,800 seconds and 20 USD of managed model-token estimates. The original Node contracts and constraints were retained. Those estimates do not cover every possible external harness/tool fee.

The numbers branch produced an actual artifact, passed its original contracts and received an approved handoff. Its real model review consumed the bound runtime observations and did not add the old controller-ack prerequisite. The text branch failed during initialize; actual harness stderr reports `database is locked`. This also occurred with an already initialized profile. The database path and exact locking mechanism have not been established.

The current-Goal TUI input “Why did the text branch fail to start? Pause the current Goal and retain all partial results.” was persisted as operation `b62f0d09-e99a-4cb8-8bf5-079f981da4a1`. Actual System One returned a valid native outcome, reasoning used the bound runtime facts, and runtime saved Goal Pause with no requirements changes or reexecution. The answer reported the recorded startup failure and requested missing error logs instead of inventing its cause. Clarification kind is Query. The query did not contain the captured stderr, so the model did not receive a database-cause fact.

The Goal remains paused and unfinished. This is not proof of two successful overlapping workers, running branch-edit adoption or combined Node delivery. Private read-only capture retains the numbers run's 270 events and the failed text startup's three events, both complete at capture time. Earlier failed Goals remain unchanged; there is no replay of their unknown effects.

## Next boundary

Implement bounded startup recovery only from authoritative evidence that no Goal prompt was admitted and owned startup teardown is confirmed, preserving original identities, partial state and budget/control fences. Failed status or a matching error string alone cannot prove safety; legacy rows without the evidence remain unknown. Do not add OpenCode-specific error matching or weaken the parallel/input-adoption/original-test requirements.

Long-lived TUI history remains a separate open issue: routine accounting/budget facts are still rendered into an unbounded event/document history. This was also visible in the real TUI capture and must not be called complete from these backend checks.
