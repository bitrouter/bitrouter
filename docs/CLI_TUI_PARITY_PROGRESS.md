# CLI/TUI parity — build progress

Base: `origin/claude/mcp-drop-complete` @ 07a2096d
Branch: `claude/cli-tui-parity-impl`
Spec: [`CLI_TUI_PARITY_BUILD_SPEC.md`](CLI_TUI_PARITY_BUILD_SPEC.md)

## Current

Task: T2
Sub-step: none
Consecutive non-green iterations on this task: 0

## Ledger

- [x] T0 — preflight and branch
- [x] T1 — ActionSpec columns, enums, eight rows, G2, G3
- [ ] T2 — G6, the HTTP profile guard read from the table
- [ ] T3 — the resolver, /commands, /help, /route reset
- [ ] T4 — SessionPorts, /status, G4, G5, A1
- [ ] T5 — /models and /preview
- [ ] T6 — CommandsReport and its builder
- [ ] T7 — bitrouter acp commands, /commands through the shared report
- [ ] T8 — the prompt-expansion registry
- [ ] T9 — acceptance sweep

## Notes

- **T0** — all ten §1 anchors verified at `07a2096d`; the stack (#869, #870,
  #875) is still open, so the branch is cut from the stack tip, not `main`.
  Baseline `cargo fmt -- --check` and `cargo clippy --all-features
  --all-targets` clean — clippy's only warning is a cargo-level
  future-incompat notice about the third-party `proc-macro-error2` crate, not
  a lint on workspace code, and it is pre-existing on the stack tip.
  `cargo nextest run --all-features`: 3031 passed, 0 failed, 11 skipped. The
  three spec docs were brought over from `claude/cli-tui-parity-spec-docs` and
  indexed in `docs/README.md` — inserted rather than overwritten, because the
  stack tip's README carries an `ACTIONS_SPEC.md` entry (from #869) that the
  docs branch, cut from `main`, does not have.

- **T1** — eight rows, `Effect`/`Requires`/`Reach`, G2 and G3. Confined to
  `crates/bitrouter-mcp/src/actions/mod.rs`; `main.rs` untouched, so the three
  pre-existing guards pass unchanged in text. 3033 tests pass (baseline 3031,
  +2 for the new guards). All five guard clauses provoked and confirmed to
  name the offending row.

  Worth carrying forward: two provocations first reported PASS because the
  mutation never applied — `cargo fmt` had reflowed
  `Effect::Write { inverse: .. }` onto three lines after the row was written,
  so patterns matching the pre-format text hit nothing. A provocation that
  silently no-ops is indistinguishable from a guard that works. The harness
  now asserts the file changed before running the test; do the same for G1,
  G4, G5 and G6.

## Blocked

(empty)
