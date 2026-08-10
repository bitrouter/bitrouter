# The `launch --tui` fidelity matrix

Status: **layers 1–2 automated; layer 3 the manual pass** · Gate for #782

Hosting eight full-screen TUI apps inside a nested emulator is the real
engineering risk in `bitrouter launch --tui`. This is how that risk is checked,
and — as importantly — what the check does *not* cover.

## The two failure sources

They need different machinery, and conflating them is what made the original
48-check matrix look both essential and unaffordable:

| Failure | Caught by | When |
|---|---|---|
| **We** break the emulator | layers 1–2 | every `cargo test` |
| **Upstream** changes rendering and breaks the wrapper | layer 3, and users | on harness release |

A recorded fixture is frozen bytes, so it can only ever catch the first. No
amount of fixture work substitutes for running the current harness.

## Layer 1 — input conformance (automated)

`apps/bitrouter/src/tui/conformance.rs`. Hosts `cat -v` under the real
`PtyPane` with the line discipline in raw mode, so everything the wrapper sends
is echoed back in caret notation and asserted against the rendered grid.

This is the direction fixture replay cannot reach. Four of the six original
matrix rows are input-path behaviours, and they fail *silently* — the wrapper
shipped once with mouse capture and bracketed paste never enabled, and nothing
could have caught it.

| Row | Assertion |
|---|---|
| arrow forwarding | `^[[A` normally, `^[OA` after the child sets DECCKM |
| bracketed paste | `^[[200~`…`^[[201~` only once the child sets DEC 2004 |
| mouse | SGR `^[[<0;3;4M` reaches a tracking child; nothing reaches one that isn't |
| interrupt | `Ctrl-C` arrives as `^C` at the child, never quitting the wrapper |
| resize | the child's own `stty size` reports the new window |

**Validated by mutation.** Disabling bracketed paste and ignoring
application-cursor mode in `term.rs` each fail exactly one test, with a message
naming the bug. A test suite that has never been seen to fail is not evidence.

## Layer 2 — recorded harness output (automated)

`apps/bitrouter/src/tui/fixtures/harness-*.vt`, replayed by
`term.rs::fixture_replay`. Real byte streams from all eight catalog harnesses,
fed in 13-byte chunks because an escape sequence split across a read boundary
is a classic emulator bug that whole-buffer feeding never surfaces.

Assertions are deliberately **loose**: no escape or C0 byte leaks into rendered
cell text, the grid keeps its dimensions, something renders. Pinning content
would turn this into a tripwire for upstream edits rather than for our
regressions.

The test is directory-driven — drop a new `.vt` in and it is covered with no
code change. See `fixtures/README.md` to record one.

## Layer 3 — the manual pass

What genuinely cannot be automated: a physical mouse drag, and a clipboard
round-trip into the host OS. Run per harness, and record the result below.

```bash
bitrouter launch -a <harness> --tui
```

1. **Mouse drag** — in a harness that tracks the mouse, click and drag to
   select. The inner app should respond; the selection should not be the outer
   terminal's.
2. **OSC-52 copy** — trigger a copy inside the harness, then paste into an
   unrelated app. The wrapper relays the sequence; the outer terminal performs
   the copy.

Sanity checks worth doing in the same sitting, though layers 1–2 cover their
mechanics: resize the window mid-session, page back through scrollback, and
run once under `NO_COLOR=1`.

| Harness | Version tested | Mouse drag | OSC-52 copy | Notes |
|---|---|---|---|---|
| claude | | | | |
| codex | | | | |
| opencode | | | | |
| pi | | | | |
| hermes | | | | |
| openclaw | | | | |
| grok | | | | own-auth: bar must read `not routed · not metered` |
| agy | | | | own-auth: same |

## The gate (redefined)

The original gate — `--tui` stays opt-in until the full matrix runs clean
across several harness releases — was circular: clearing it required standing
machinery that only pays off if someone owns triaging it. A weekly job that
goes red on an upstream change and is ignored for a month is *worse* than no
job, because it launders "unverified" into "we have CI for that."

**`--tui` clears the gate when:**

- [ ] layers 1 and 2 are green in CI (they are, and they run on every commit)
- [ ] layer 3 has been run once against all eight harnesses, with the table above filled in
- [ ] the flag has been opt-in through at least two release cycles with no unresolved rendering reports

Scheduled live-harness smoke (installing the latest of each and asserting
sanity under the wrapper) is **deliberately not built**. It is the only thing
that detects upstream regressions automatically, and it is worth building the
day someone reports a harness-specific rendering bug — that report is the
evidence that the recurring cost is justified. Until then the flag is opt-in,
the population is self-selected, and a bug report is the cheaper detector.

**Permanently manual:** `grok` and `agy` are proprietary, auth-gated
subscription clients. They cannot be installed in CI, so no automated tier will
ever cover them.
