# Superseded execution archive: current local slice

Base: `067ef9249f06efbdbfb48bdddbbf3e32e6f1b2ea`. This slice is local and
uncommitted, not included in the published PR head's successful CI.

The preceding Goal turn added the native campaign observer and diagnostics,
compiled them and ran strict Clippy. Its V17 maintained campaign subsequently
finished with both cases failing: Claude exceeded its original total workload
deadline at trial 220 (199 baseline, 20 challenger), before adoption; Codex
timed out while reading its learning snapshot. The observer change has not
established successful publication/rollback acceptance. The complete terminal
results are recorded in `2026-10-14-native-campaign-observer.md`.

## Authority and storage

`workflow::Change::ArchiveExecution` commits the complete original execution,
all its retired permission records and a content-addressed record reference
to the existing immutable Goal operation history. The current head removes
only that exact original execution and its inactive permissions, increments
the task's archive count and retains the newest record reference. The record
contains the previous archive reference. No workspace/artifact bytes, actual
tool evidence, cumulative budget or accounting are erased.

The stable receipt identity is derived from Goal and execution identity.
Archive retries return the original operation, and new attempts cannot reuse
an archived execution ID even in another task. Head and complete receipt share
the existing SQLite transaction; failure leaves the head intact. A record file
created before an unsuccessful transaction is not authoritative and cannot
authorize effects. Queries retain their existing owner boundary.

Only non-latest executions from superseded task revisions with confirmed stop
and verified reconciliation may leave the head. Pending checks/review,
unresolved effect or permission outcomes, active completion/previous delivery
sources, matching in-flight accounting and unresolved input references pin
records. Any pending input also pins history while interpretation runs.
Current-version attempts remain in the head, preserving the three-attempt
admission rule. The 16-entry hot retention bound is not increased.

The daemon archives at most one eligible execution per workflow pass, then
reads the committed head and continues independent work. The readonly task
inspector presents the archive count and a hashed record artifact; browsing it
does not acquire control, answer permissions or start execution.

New serialized history events require Goal IPC v4. The legacy outbox regression
now covers v1 and the previously published v3; both remain retained/rejected
without reinterpretation or replay. Native local v17 and native/external ACP
are unchanged. CLI reference, development contract and shipped BitRouter skill
are synchronized; no public CLI flag, port, provider credential or harness
wiring step changed.

History pagination now includes its serialized envelope and separators in the
byte budget. A record that cannot fit a page returns an explicit error naming
its original operation identity, rather than an empty non-advancing page.

## Executed checks and their limits

- Initial test compilation caught missing fixture session identity and an
  invalid comparison of `GoalReply`; only tests were corrected.
- V2 ran three cases and rejected fixture changes lacking a recorded reasoning
  request. The production guard was preserved; the domain fixture now supplies
  an explicitly labeled fixture request identity. This is not a real model call.
- V3 passed three archive tests. V4 passed 76 Goal database tests but marked
  two cases LEAK. The same two cases passed serial recheck without LEAK.
  The exact cause is unproven. Nextest's [leak documentation](https://www.nexte.st/docs/features/leaky-tests/)
  explains the output-handle condition; it does not diagnose these local runs.
- V5 passed 77 Goal database cases without LEAK. Its following Clippy pass
  found two errors in the newly edited daemon test (out-of-scope original ID
  and an unqualified enum); both are corrected in current source. That run
  is not an immutable final-source claim.
- Current V6 freezes 108 source files. Owned session 89454 runs actual
  coordinator/client/protocol tests, the complete Goal database suite, full
  workspace tests, strict all-target/all-feature Clippy, separate doctests,
  formatting, diff and source hashes. Results remain pending until terminal.

The V6 coordinator/client/protocol run passed 19 cases in 130.484 seconds,
four slow, no LEAK or timeout. This includes the actual daemon resource-edit
and archived-original read verification, old-protocol outbox refusal,
active independent branches, paused edits and unknown-tool recovery refusal.
The later full checks are still running; this result does not imply their success.

The same frozen V6 source then passed all 77 Goal database cases:
`25797c94-e7a4-47e0-8684-327e7d4add14`, 1.560 seconds, zero skipped,
no LEAK or timeout. The complete workspace suite and later checks are still
running on session 89454; their outcome remains independent.

That V6 pipeline is now terminal success (exit 0). Workspace run
`150db128-a073-4c5f-9ed3-d7f155189927` passed all 4,606 tests, 22 original
opt-in skips, nine slow, 681.962 seconds, no LEAK or timeout. Strict workspace
Clippy passed in 1m11s; separate doctests passed six with one original ignored.
Formatting, diff and 108 source hashes passed. A later resource-current-reader
change has a separate manifest and does not inherit these source-bound checks.

The tests preserve fault rollback, matching identity replay, cold reopen and
owner scope; nonzero cumulative accounting under a halted budget; active
reference/unknown-effect refusal; current-version retries; and 24 requirement
cycles with more than 1 MiB of complete cold execution history. A page-boundary
case checks all 16 accepted original inputs survive bounded pagination.
Execution and permission facts in these storage tests are explicit domain
fixtures, not harness or real-model evidence. The actual daemon resource-edit
test additionally reads the archived original through the local operation
query and verifies the hashed record and replacement resources.

This slice does not establish the complete long-history UX, general external
effect recovery, global external-charge budgeting, real input evaluation or
authenticated typed-provider gate. The full G/A/F completion audit remains
open. No Goal-complete claim, push, merge, release or deployment is made.
