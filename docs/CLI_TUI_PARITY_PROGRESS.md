# CLI/TUI parity — build progress

Base: `origin/claude/mcp-drop-complete` @ 07a2096d
Branch: `claude/cli-tui-parity-impl`
Spec: [`CLI_TUI_PARITY_BUILD_SPEC.md`](CLI_TUI_PARITY_BUILD_SPEC.md)

## Current

Task: complete
Sub-step: none
Consecutive non-green iterations on this task: 0

## Ledger

- [x] T0 — preflight and branch
- [x] T1 — ActionSpec columns, enums, eight rows, G2, G3
- [x] T2 — G6, the HTTP profile guard read from the table
- [x] T3 — the resolver, /commands, /help, /route reset
- [x] T4 — SessionPorts, /status, G4, G5, A1
- [x] T5 — /models and /preview
- [x] T6 — CommandsReport and its builder
- [x] T7 — bitrouter acp commands, /commands through the shared report
- [x] T8 — the prompt-expansion registry
- [x] T9 — acceptance sweep

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

- **T5** — `/models [provider]` and `/preview <model>`, D8(b) as decided.
  `SessionPorts` gains its other two ports *with* their `run` arms (T4's
  deferral discharged). A1 now covers all three reads. 3046 tests pass (+4).

  **The 60-column check found a defect, not a fit problem.**
  `ModelsReport::render` emits a literal tab between its columns — deliberate,
  so `bitrouter models --human | cut -f1` works. On a ratatui screen that is
  unsafe: the differential writer measures rows with `unicode-width`, where a
  tab is one column, while the terminal advances to the next tab stop, so every
  row after the tab is misplaced. It appears in the *narrow* common case
  (`demo-model<TAB>demo`); in wide output the tab happens to land on a wrap
  boundary and hides.

  Fixed at the seam rather than by taking the spec's "drop the `tui_command`"
  fallback: `plain_lines` expands tabs to the next eight-column stop, which is
  what the terminal would have done. This is the seam's job, not the extra
  renderer §5 warns against, and it protects any future report that uses tabs.
  On width proper both commands wrap readably at 60 columns even on a hostile
  catalog (6 providers, 48-char model ids), so `list_models` keeps its
  `tui_command`.

  For T6/T7: any report rendered into a notice must be checked for tabs the
  same way. `CommandsReport` should avoid them entirely.

- **T6** — `CommandsReport`, `commands_report`, and the `CliReport` impl.
  Types and builder only; the `commands` row still carries `cli_leaf: None`
  and `output_schema: None` — T7 sets both. 3051 tests pass (+5).

  Two deviations, both CLAUDE.md rule 4: `commands_report` takes three
  arguments, not the spec's four — the `config` slice needs `PromptCommand`,
  which does not exist until T8, so it arrives there with its producer (T7
  therefore calls it with three). And `CommandRow` gained an `unavailable`
  field the spec did not list, so a listed-but-unrunnable command keeps its
  reason through the report rather than losing it at the boundary.

  The rendering is deliberately tab-free, with a test pinning it — the direct
  consequence of what T5 found.

- **T7** — `bitrouter acp commands` leaf, `/commands` on the shared report,
  `commands_received` on the journal, `hint` carried through `translate.rs`,
  and T3's scaffolding renderer deleted (77 lines + 78 of its tests, whose
  coverage moved to `commands_report` in T6). 3046 tests pass — five fewer
  than T6 by exactly those deleted renderer tests. All four table guards pass;
  `acp commands --help` resolves.

  Three judgment calls:

  1. The raw-update subscription is opened **before** `new_session`, not
     after. An agent that advertises immediately would otherwise race it and
     be reported as silent — which is the very distinction `received` exists
     to draw, so the obvious ordering would have made the flag lie.
  2. `--source` has its own clap enum rather than a `ValueEnum` derive on
     `CommandSource`. The report is a schema shared with the MCP surface;
     a CLI concern does not belong in it.
  3. `render/session.rs`'s module doc claimed it renders
     `AvailableCommandsUpdate` "on request" — true when T3 wrote it, false
     once the rendering moved app-side. Corrected.

  Process note: a piped `cargo build ... | grep | head` reported `rc=0` while
  masking a real compile error. Count `^error` lines; do not read the exit
  code through a pipe.

- **T8** — `chat.commands` in `bitrouter.yaml`, `$ARGUMENTS` expansion shared
  by the TUI, the piped loop and `acp prompt`, and the load-time collision
  check. `commands_report` gained its `config` group (T6's deferral
  discharged). 3050 tests pass (+4).

  The structural point: the two registries are never held in one collection.
  `State` carries `commands` and `prompt_commands` as separate fields and the
  closed set is consulted first, which is sound only because the open set
  cannot contain a name the closed one has — refused once at load. That is
  what keeps G1-G3 exhaustive over `ACTIONS` while saying nothing about
  config. It cannot live in the SDK: it needs `ACTIONS`, and `bitrouter-mcp`
  depends on `bitrouter-sdk`, not the reverse.

  Placement: the check runs before the terminal is taken, so a bad config
  fails as plain text rather than from inside a raw-mode screen, and
  `config validate` runs it too. `acp prompt` passes an empty BitRouter half
  to the resolver deliberately — only the expansion is shared.

  Process note, third variant of one mistake: a test filter that printed
  neither PASS nor FAIL was read as success when the compile had actually
  failed (the tests used `serde_yaml`, not a dependency; they now use the
  crate's own `parse()`). Together with T1's no-op provocation and T7's
  `rc=0` through a pipe: assert on a positive signal, never on the absence of
  a negative one.

- **T9 — acceptance sweep.** §7 run in full.

  | Line | Result |
  |---|---|
  | fmt / clippy / nextest clean at **every** commit in the range | **was recorded wrong.** Every check grepped clippy for `^error` only, so warnings were invisible; clippy warns rather than errors by default. The branch carried 4 warnings it introduced (base had none). Corrected in the review-fix commit; clippy is now clean of warnings *and* errors |
  | G1, G2, G3, G6 from T3's commit; G4, G5, A1 from T4's | pass — 10 guards run green; each provoked once when written |
  | the three pre-existing guards unchanged **in text** | pass — byte-identical to base (15 / 24 / 26 lines) |
  | `multitenant_http.rs` unchanged | pass — 0 diff lines vs base |
  | `bitrouter-tui` gained no `bitrouter-*` dependency | pass — 0 added |
  | session notice bytes == `render_to_vec` of the same report | pass — **live**: piped `/status` against `claude-acp` is byte-identical to `bitrouter status --human` (7 lines) |
  | `acp commands` against a real harness | pass — 6 bitrouter rows + 70 agent rows, hints rendered, `received: true` |
  | `acp commands` against an empty-list and a silent harness | **not run live** — no stub harness available. Covered by unit tests (`silence_and_an_empty_list_are_different_answers`) |
  | by-hand: routed chat, `--direct`, piped, at 60 columns; `stty` restored | **partially run** — the piped path was exercised live and exits 0. The two interactive paths need a TTY, which this session does not have. Not verified |

  **Finding, not fixed (§7 says record and stop).** 4 of 76 agent command
  descriptions from `claude-acp` contain embedded newlines, so
  `impl CliReport for CommandsReport` spills past the `  /name  description`
  indent onto unindented continuation lines. Not corruption — `plain_lines`
  splits on newlines, so the differential writer's arithmetic stays correct —
  but the rendering assumes single-line descriptions and ACP does not promise
  that. Fix is one line (take the first line, or indent continuations).

- **Review fixes (post-T9).** A `feature-dev:code-reviewer` run on fable found
  six defects; all fixed, plus the clippy warnings the sweep had missed.

  | Severity | Defect | Fix |
  |---|---|---|
  | critical | `acp commands` leaked the harness child: `prompt_commands(..)?` sat between `launch_controlled` and `shutdown()`, and `ControlledSession` has no `Drop` | hoisted above the launch |
  | important | `/status`, `/models`, `/preview` appeared only on the *next* keystroke — `Resolution::Action` returned before `Effect::Paint`. The route arms get away with it because their wire replies paint | dropped the early return; added a `submit()` test over every arm, provoked |
  | important | `acp commands` passed `binding: None`, so it reported `/route` unavailable for every agent while `chat` offered it — the exact drift the shared report exists to prevent | opens the same binding `chat` does; verified live, all six rows now AVAILABLE |
  | important | `chat.commands` accepted `name: /review`; the resolver strips one slash and never matched, so it went to the agent and listed as `//review` | rejected at load with the corrected name in the message |
  | minor | `&& row.id != ""` — dead conjunct, clippy `comparison_to_empty` | removed |
  | minor | `commands` was inserted between the `── prompt ──` banner and `pub async fn prompt`, so rustdoc attached prompt's docs to it | moved after `prompt` |

  Three `too_many_arguments` warnings came from threading `commands`,
  `prompt_commands` and `ports` separately. `#[allow]` is forbidden, so they
  are bundled as `SessionSurface` — one concept, the session's command
  surface. `run` 8→6, `drive` 9→7, `chat_piped` 8→7.

  **The lesson the sweep itself missed.** The paint bug survived because the
  resolver tests exercised `resolve()` directly and nothing exercised
  `submit()`. A unit test of the pure function proved the mapping and said
  nothing about the effects the reducer emits around it.

## Blocked

(empty)
