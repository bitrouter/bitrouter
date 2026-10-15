# Durable model acknowledgement and explicit input withdrawal

Model-control failures now distinguish source revision, budget, run intent,
completed version, missing/transferred/expired coordinator lease and accounting
operation acknowledgement. The first callback acknowledgement failure is
retained in a bounded diagnostic; an unacknowledged model result is still
unusable. Actual fixture timeout output also retains accounting/budget state.

The red run injected five commit conflicts and an actual SQLite commit whose
reply was lost: one test passed, two failed, exit 100. These reproduce two
concrete causes of the previous opaque error, not the specific historical
hosted Linux cause. Confirmation now retries the identical accounting fact
and operation identity with bounded backoff. It does not repeat a model call,
preparation or tool effect. Confirming the original receipt does not duplicate
work or settlement. Final storage failure remains fenced; a successful Begin
receipt is followed by a fresh authority check so pause/ownership transfer
cannot turn a historical intent into current dispatch permission.

Ack-only v2 passed 10 selected cases. v3 passed 84, including injected conflicts,
lost Begin/outcome replies, persistent storage failure, pause after commit,
ownership transfer and all original actual coordinator/domain/Goal PTY cases.
This is local evidence; the published PR remains `5a18a35e`, whose hosted Linux
model source/owner/ack failure is not yet confirmed repaired.

`WithdrawInput` directly withdraws one unresolved request under the current
Goal, original input/source version and current-head CAS. Requirements,
controls, budget, accounting, worker/tool effects and other accepted/in-flight
input order remain. Receipt and disposition are committed atomically. Exact
`/input <id> /withdraw` uses the typed path and existing client outbox, without
a model, additional cap or coordinator lease. Quotes, hypotheses and extra
words retain ordinary understanding. A separate admission lane follows the
same original identity through response loss and cannot leave the input lane
stuck. Goal protocol v3 rejects older outboxes without replay/reinterpretation;
native/ACP protocols are unchanged.

Withdrawal v1/v2 did not execute tests because of two test helper import/name
errors. v3 passed 95 and failed four existing PTY cases: their helper still sent
Goal protocol v2 and failed readiness against v3. Both hardcoded instances were
replaced by the protocol constant without changing the original deadline.
The active PTY now also submits an unavailable resource request, receives an
actual clarification and withdraws it with Enter. Its typed receipt and backend
capture check that the explicit withdrawal does not become a model request.
The v4 source is independently frozen and tested.

Provider/model judgments in transport fixtures are fixed. These tests do not
establish real semantic quality, real credentialed providers/harnesses or
complete G/A/F acceptance. These results were initially local; the subsequent
explicit PR request authorizes publishing them to the existing draft.

Withdrawal v4 passed all 99 selected cases, exit 0, 111.989 seconds, four slow
cases, no LEAK or timeout, 2,195 filtered. In the actual active Goal PTY, Enter
withdraws the real unavailable-resource question. History contains the typed
InputWithdrawn receipt and no ordinary Input containing that command; backend
capture contains no model request for it. Requirements, run intent, control
source and budget remain, both workers stay active, and the original permission,
read-only inspector, unsent draft, detach and verified-delivery checks pass.

Actual SQLite rollback and idempotency tests preserve other questions and the
accepted input cursor. An already verified delivery can complete after an
explicit withdrawal under the exhausted cap without another model or cap
increase. The real client/outbox case preserves exact draft/identity through
recovery and releases the withdrawal lane after its matching receipt. These
selected results precede the full-workspace/static/doc source check.

Final v4 workspace run `60bdf38a-bb23-46f9-856e-a70f5d1cda9a` passed all
4,598 tests across 71 binaries, 22 existing opt-in skips, 666.968 seconds,
nine slow cases, no LEAK or timeout. Combined session 52175 exited 0 after
strict workspace/all-target/all-feature Clippy (70 seconds), separate doctests
(six passed, one original ignored, seven suites), formatting, diff checks and
all 103 source hashes. Normal deadlines and pressure groups were retained.

Before the PR update, only the stale bare-entry source comment and development
protocol table were corrected. The separate PR source manifest records those
two changes; executable behavior is unchanged. Six opt-in evolution scenarios
still use the retired `code <agent>` entry and remain an explicit migration
gap. The draft is not complete MVP acceptance or ready to merge.
