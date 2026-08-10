# VT fixtures — the automated tier of the fidelity matrix

Hosting eight full-screen TUI apps inside a nested emulator is the real
engineering risk in `bitrouter launch --tui`, and it is **not** a one-time
risk: harness releases are frequent and outside our control, so an upstream
rendering change can regress the wrapper without either project noticing.

A 48-check manual matrix (8 harnesses × 6 behaviours) does not get re-run. So
the matrix is split into three tiers, and this directory is the first one.

| Tier | Covers | Runs |
|---|---|---|
| **Fixture replay** (here) | colors, `NO_COLOR`, alt-screen entry/exit, mouse-mode negotiation, bracketed paste, layout | every `cargo test` |
| CI smoke | "starts at all under a PTY" — each harness's `--version` inside the wrapper | CI, 6 of 8 (grok and agy are proprietary and unavailable there) |
| Manual | real mouse drag into a mouse-reporting app; OSC-52 copy reaching the outer terminal | by hand, per harness |

## What is here now

Synthetic fixtures covering the emulator behaviours the wrapper depends on.
They are deliberately small and readable, and they pin our own regressions —
if someone changes `term.rs` and alt-screen tracking breaks, these fail.

## What is still needed: real harness recordings

These synthetic fixtures **do not** prove any particular harness renders
correctly. For that, record each harness once and drop the capture in here:

```bash
# macOS / BSD
script -q /dev/null claude 2>&1 | tee claude_session.vt
# GNU/Linux
script -q -c claude claude_session.vt
```

Then add the file to `REAL_HARNESS_FIXTURES` in `../term.rs`'s replay test.
Re-record after a harness release that changes its rendering; that re-recording
is what converts an upstream regression from "a user reports it" into "CI
fails".

Recordings are byte streams, not transcripts — scrub them before committing if
a session contained anything private.
