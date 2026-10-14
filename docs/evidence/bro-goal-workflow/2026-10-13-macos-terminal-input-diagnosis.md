# macOS resize and keyboard input diagnosis

This is separate from the earlier loader problem, which no longer reproduces.
It concerns actual PTY input to `bro code` during a running, parallel Goal.

## Reproduction and distinguishing observation

The active Goal test uses the real CLI, daemon, database and two ACP fixture
processes. Provider judgments are fixed fixture replies; this is terminal and
runtime evidence, not model-quality evidence.

- The uninstrumented v6 regression ran 72 related tests: 71 passed and the
  active Goal test failed. Run `d47c98ac-115b-433c-9390-f44a4d685608`, exit 100.
- A temporary event trace confirmed that the final Enter and Ctrl-Left were
  absent before `State.event`; earlier Ctrl-Left correctly navigated to Goals.
- The distinguishing run `ce308c99-c645-439e-be71-19f9e6a29578` logged successful
  `write_all` and `flush` of Enter and Ctrl-Left. The UI continued drawing, but
  neither key reached the event trace within the unchanged 15-second deadline.
- After that failure, the diagnostic sent only Ctrl-L. The old Enter, old
  Ctrl-Left and new Ctrl-L then appeared in order, and the screen became Goals.
  The original failure was returned unchanged: 0 passed, 1 failed, exit 100.

See the [distinguishing run](2026-10-13-goal-viewport-permission-drain-probe-nextest.log).
The original failures are retained; extra input was a diagnostic, not a retry
used to make the test pass.

## Source mechanism and proposed correction

Crossterm 0.28.1's Unix mio source returns immediately for the first parsed key,
resize or wake token, without preserving unprocessed readiness tokens from the
same poll result. In particular, a resize can return before TTY readiness is
handled. Mio's macOS kqueue registration uses `EV_CLEAR`; its documented contract
requires draining readable I/O to `WouldBlock`, otherwise a later readiness
notification is not guaranteed. The delayed keys appearing after new input
strongly support this mechanism; this is not a kernel-level trace proving which
token order occurred in that particular poll.

Primary source references:

- [Crossterm Unix mio reader](https://docs.rs/crate/crossterm/0.28.1/source/src/event/source/unix/mio.rs)
- [Crossterm pollfd reader](https://docs.rs/crate/crossterm/0.28.1/source/src/event/source/unix/tty.rs)
- [Mio polling contract](https://docs.rs/crate/mio/1.2.1/source/src/poll.rs)
- [Mio kqueue registration](https://docs.rs/crate/mio/1.2.1/source/src/sys/unix/selector/kqueue.rs)

The app's macOS dependency enables `use-dev-tty` and `libc` on the same Crossterm
version. The first candidate, `use-dev-tty` alone, failed compilation with
E0599: its event-stream wake branch unconditionally calls `FileDesc::new`, which
exists only under `libc` in this version. That run executed no tests and is
retained in the [compile-failure log](2026-10-13-goal-viewport-permission-pollfd-paired-nextest.log).
This selects its level-triggered Unix pollfd reader, which handles TTY before
the resize pipe. The event API, parser, Goal semantics, deadlines and permission
assertions stay the same. Other target configurations are unchanged; their
execution still requires their own platform verification.

## Independent fixture synchronization issue

`resize_and_wait` previously accepted any new complete terminal frame, including
an already queued frame. It also did not wait for the preceding key group to be
consumed. The trace proves that some keys sent at 30x10 were processed only
after the terminal became 40x12 or 100x24. This does not invalidate the production
hidden-selection bug or the lost-readiness observation, but requires a separate
test barrier before the next resize. A fresh explicit redraw of the stable
small-window warning can acknowledge consumption of the preceding input group.

The paired pollfd result, final fixture barrier and removal of all temporary
production tracing are recorded in the implementation ledger after validation.

## Terminal restoration comparison

The paired reader-only run passed: run
`0367f367-dab3-434f-ab49-89e222b17780`, 1 passed, exit 0, 94.598 seconds.
After removing tracing and adding the consumption barrier, the related v2 run
reported 70 passed and 2 failed; the active Goal passed, while suspension/editor
checks failed their debug-string comparison of terminal modes.

The [complete original/actual comparison](2026-10-13-goal-pollfd-termios-comparison-nextest.log)
shows the sole difference: `c_lflag` 1483 became 536872395, exactly `PENDIN`.
All other flags, control characters and speeds matched. Apple’s published
[XNU tty source](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/tty.c)
sets this temporary bit when restoring `ICANON` and retains it during assignment;
the driver clears it on subsequent pending-input processing. This source
explains the observation, without asserting that public main is byte-identical
to this machine's kernel.

The fixture now compares typed complete termios values, normalizing only macOS
`PENDIN` in both snapshots. Echo, canonical input, signals, all other flags,
control characters, input/output speeds and platform fields remain exact.
There is no terminal mutation or production restoration change to force the
comparison to pass. The first normalizer changed Nix's public flags, but that
version's `From<Termios>` extracts the unchanged inner snapshot directly; v3
therefore still failed with precisely the same raw difference. V4 normalizes
the complete raw value's `c_lflag` instead. All other raw bits and fields are
preserved, including any flags not represented by Nix's public enums. Final
v4 validation is recorded separately.

Final uninstrumented v4: 72 related tests passed, exit 0, run
`43515600-7e2a-4f66-a031-205aa142ba82`, 111.102 seconds; 2484 tests were filtered
out. Workspace/all-target/all-feature Clippy with warnings denied, formatting,
diff and 98-file source identity checks passed. Separate workspace doctests:
6 passed, 1 ignored, exit 0. No owned test processes remain. These are local
fixture/static results, not complete MVP or actual-provider certification.
