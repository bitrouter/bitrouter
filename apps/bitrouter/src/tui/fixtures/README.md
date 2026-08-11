# VT fixtures — the automated tier of the fidelity matrix

Hosting eight full-screen TUI apps inside a nested emulator is the real
engineering risk in `bitrouter launch --tui`, and it is **not** a one-time
risk: harness releases are frequent and outside our control, so an upstream
rendering change can regress the wrapper without either project noticing.

A manual matrix of every harness × every behaviour does not get re-run. So
it is split into layers, and this directory is one of them.

| Layer | Covers | Runs |
|---|---|---|
| Input conformance (`../conformance.rs`) | keys, mouse, paste, interrupt, resize — everything the wrapper *sends* | every `cargo test` |
| **Fixture replay** (here) | colors, `NO_COLOR`, alt-screen, mouse negotiation, real harness output | every `cargo test` |
| Manual | physical mouse drag; OSC-52 copy into the host clipboard | once per harness, per `docs/TUI_FIDELITY_MATRIX.md` |

Scheduled live-harness smoke is deliberately not built; see the matrix doc for
why, and for what that leaves uncovered.

## What is here

- **Synthetic** (`main_screen_colors`, `alt_screen_app`, `mouse_reporting`,
  `bracketed_paste`) — small and readable, each pinning one emulator behaviour
  the wrapper depends on.
- **`harness-*.vt`** — real byte streams from the four harnesses `launch`
  supports (claude, codex, opencode, pi).

## Recording a new one

```bash
scripts/record-vt-fixture.sh <name> <command> [args...]

scripts/record-vt-fixture.sh harness-codex-help codex --help
scripts/record-vt-fixture.sh harness-codex-session codex     # interactive; quit when done
```

The script pins a 100x30 pty so replays are deterministic. **No code change is
needed** — `term.rs`'s replay test discovers every `harness-*.vt` in this
directory, which is what keeps re-recording cheap enough to actually happen
after a harness release.

## Before committing a recording

These are raw byte streams, not transcripts. A real session capture contains
whatever was on screen — prompts, paths, file contents, anything a tool
printed. Read it first:

```bash
LC_ALL=C strings apps/bitrouter/src/tui/fixtures/<name>.vt | less
```

The committed `--help` captures were scanned for credentials, home paths, and
usernames; their only matches are help text *documenting* flag names such as
`ANTHROPIC_API_KEY`, not values.

## What these prove, and what they do not

They catch **our** regressions against real-world output. They cannot catch an
**upstream** one: a fixture is frozen bytes, so it keeps passing while a new
harness release breaks the live wrapper. Only re-recording, or running the
harness, closes that gap — see `docs/TUI_FIDELITY_MATRIX.md`.
