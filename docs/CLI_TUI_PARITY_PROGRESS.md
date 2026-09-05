# CLI/TUI parity — build progress

Base: `origin/claude/mcp-drop-complete` @ 07a2096d
Branch: `claude/cli-tui-parity-impl`
Spec: [`CLI_TUI_PARITY_BUILD_SPEC.md`](CLI_TUI_PARITY_BUILD_SPEC.md)

## Current

Task: T5
Sub-step: none
Consecutive non-green iterations on this task: 0

## Ledger

- [x] T0 — preflight and branch
- [x] T1 — ActionSpec columns, enums, eight rows, G2, G3
- [x] T2 — G6, the HTTP profile guard read from the table
- [x] T3 — the resolver, /commands, /help, /route reset
- [x] T4 — SessionPorts, /status, G4, G5, A1
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

- **T2** — G6 added inside `http_profile_never_carries_host_bound_tools`;
  both existing literal assertions kept, so the change is strictly additive.
  Confined to `server.rs`. Provoked by re-marking `list_models` as
  `HostBound`: fails naming the row and both `Reach` values. 3033 tests pass
  (count unchanged — G6 is an assertion inside an existing test, not a new
  one). `multitenant_http.rs` byte-identical to base (invariant 3).

  Note for T3: it is the largest task in the plan and crosses the
  `bitrouter-tui` -> `apps/bitrouter` boundary. `State::new`'s signature change
  leaves the workspace red between sub-steps 2 and 7 — that is expected, and
  nothing may be committed until sub-step 10 is green. Expect 2-4 iterations.

- **T3** — `bitrouter-tui` half complete: `Command`,
  `REDUCER_OWNED`, `ALIASES`, `Resolution`, `resolve`, `State.commands`,
  `State::available`, `Effect::ResetRoute`, widened `Action::Routed`, the
  rewritten `submit`, the grouped renderer, and the picker gate reading
  `available("route_set")`. 153 tests pass. App half wired: `actions/session.rs`
  with `offered_commands`/`summary_for`, the `ResetRoute` wire arm, `run` and
  `chat_plain` taking the command list, `can_reroute` deleted.

  Two things the spec did not foresee, both resolved in-tree:

  Phase 0 is complete. 3039 tests pass (+6 over T2: two resolver tests, three
  renderer tests, G1). All four G1 clauses provoked and each named the
  offender. `bitrouter-tui` gained no BitRouter dependency (invariant 1) and
  `multitenant_http.rs` is unchanged (invariant 3).

  1. `machine.rs` already had a private `fn resolve(prompt, outcome) -> Effect`
     (permission answering, 5 call sites), colliding with the dispatcher the
     spec names `resolve`. The private helper is renamed `answer_with`; the
     public name is the spec's. `answer` alone was tried first and collided
     again with a local binding at what is now `machine.rs:564`.
  2. A blunt `\banswer\(` regex also rewrote the `Prompt::answer` *method* at
     the `decide` call site. Caught by the compiler, but it is the same class
     of error as the T1 provocation no-op: a text substitution that matches
     more than intended. Prefer anchored multi-line replacements with a
     count assertion over word-boundary regexes.
  3. G1 lives in `main.rs`, which is the *binary* root, so it reaches the
     library as `bitrouter::actions::session`, not `crate::actions::session`.
  4. `State::new` is called in `drive`, not `run`, so the command list is
     threaded through both.

- **T4** — `/status` answers in a session through the same `StatusQuery` port
  the CLI leaf and the MCP tool use. G4/G5 appended to the chat guard (nine
  original strings intact); A1 asserts the session surface produces the leaf's
  bytes, as JSON and as rendered output. 3042 tests pass (+3). D1 enacted as
  (a), narrowly: rendered once on request, never retained — G4 is the
  enforcement.

  Three deviations, each in the direction of the spec's own invariants:

  1. **The spec's "driver test with a stub `SessionPorts`" cannot exist.** G4
     forbids `session.rs` naming any report type, and a stub must name one.
     The assertion lives in `actions/session.rs` instead, testing the same
     property without the driver naming what it renders. T7 should not try to
     add a stub-based driver test either.
  2. **`plain_lines` belongs in `bitrouter-tui`, not app-side.** Written
     app-side it needs `ratatui::text::Line`, and `apps/bitrouter` has no
     `ratatui` dependency by design — the app forwards `Vec<Line>` it never
     names. It is `render::session::plain_lines`.
  3. **`SessionPorts` carries only the ports it reads.** The spec lists
     `{status, models, route}` in T4, but two have no reader until T5: a
     dead-code warning and a CLAUDE.md rule 4 breach. Same for the unused
     `from_parts`. T5 adds the two fields *with* their `run` arms. `args` got
     a reader by having `/status` refuse arguments rather than swallow them.

## Blocked

(empty)
