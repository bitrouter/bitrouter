# PR #981 Windows patch transfer repair

The hosted Windows job for head
`9b83f2cc9a4dab250bfc0c205fe34f68c94e130e` finished with 4,432 passed, two
failed and three skipped. Both artifact-transfer tests failed on all three
attempts because Git could not open the canonical Windows verbatim path to a
patch blob. Linux and macOS jobs passed that head. The exact failure excerpt
and original hosted job URL are retained in `2026-10-14-pr981-windows-failure.txt`.

Artifact application now sends the already hash-verified bytes to Git over
standard input. The initial tracked-diff workspace preparation uses the same
helper. Input writes and output draining run concurrently, the input pipe is
closed before waiting for process exit, and the existing 30-second deadline,
kill-on-drop and Git exit validation remain. Binary application, three-way
merge, base-version checks and conflicting-content rejection are preserved.
This also avoids reopening the patch file after checking its identity.

The existing transfer and diamond/conflict cases remain. A new case transfers
a tracked 192 KiB binary file through a patch larger than 64 KiB in directories
with spaces and checks exact destination bytes. Fixture repositories explicitly
disable automatic line-ending conversion for their existing byte assertions;
this does not change user repository configuration.

Run `055bb19b-0fcd-499e-908a-e2535940d8eb` passed all 12 selected artifact and
actual ACP coordinator cases, exit 0, 92.510 seconds, four slow cases, no LEAK
or timeout. This is a local macOS result, with 1,731 tests skipped by the
selection. It does not establish that the repaired Windows hosted job passed.
The final workspace/static/doc checks and new PR head are recorded separately.

The source identity is
`2026-10-14-pr981-windows-fix-source.sha256` (102 files). The source check
matches the actual resource-edit and Windows-repair code; it does not inherit
the previous resource-edit-only full-workspace result. Real provider/harness/
typed-backend evaluation, persistent clarification handling and the full
G/A/F acceptance audit remain unfinished. The PR stays Draft.

Final source run `747c41e5-135c-4832-b1bb-fcb75b821160` passed all 4,583
workspace tests, with 22 existing opt-in skips, 71 binaries, 769.264 seconds,
ten slow cases, no LEAK or timeout. Compound session 32761 exited 0 after
strict workspace/all-target/all-feature Clippy (76 seconds), separate doctests
(six passed, one existing ignored), formatting, diff checks and all 102 source
hashes. Darwin linking emitted a nonfatal compact-unwind size diagnostic; no
check was disabled. The hosted Windows result for the updated PR head must
still be confirmed independently.
