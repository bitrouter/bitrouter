# Execution plan: the TUI renderer (retained journal + differential writer)

Companion to [`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md). The spec holds the
*decisions and rationale*; this file holds the *ordered work and its completion
criteria*. Scope is the spec's **v1 column only** (§15) — every "later" row is
out of scope here.

Designed to be driven by `/goal`. See §A for the loop protocol and §E for the
goal strings.

---

## A. Loop protocol

**The `/goal` evaluator reads only the conversation transcript.** It does not
run commands and does not read files. Anything you want it to judge, you must
**print**.

Each turn:

1. Read this file. Pick the **first unchecked task whose `Depends on` are all
   checked**. Do not batch — one task per turn.
2. Do that task, and only that task.
3. Run the task's **Verify** command. Paste its output into the transcript.
4. Tick the task's checkbox in §D and commit (message under `Commit:`).
5. **Print a status line in exactly this form:**

   ```
   PLAN STATUS: phase <N> — <done>/<total> tasks complete — next: <task id or NONE>
   ```

6. If a task fails twice in a row, mark it `- [!]` with a one-line reason, print
   `BLOCKED: <id> — <reason>`, and move to the next eligible task. If every
   remaining task is blocked, stop and report.

**Do not re-litigate the spec.** Every decision in §C is settled. If you believe
one is wrong, finish the task as written, then note the objection in the
transcript — do not silently substitute a different design.

**Standing rules** (from `CLAUDE.md`, enforced per task):

- Never `#[allow(...)]` to bypass a check.
- Never `.unwrap()`, `.expect()`, or `panic!` in production paths.
- Never re-export inside a public `mod`.
- No dead code — no type, method, or trait added without a caller in this work.
- Conventional commit messages; title under 60 chars.

---

## B. Goal conditions

One phase per `/goal`, in order. Do not start a phase before the previous one's
gate task is ticked. Exact strings in §E.

---

## C. Settled decisions (do not re-open)

From the spec, restated so this file is usable alone:

1. **Retained journal, not append-only.** `Journal::apply` returns `()`
   (spec §3).
2. **Differential writer over `ratatui::backend::Backend`**, not raw crossterm
   and not `Viewport::Inline` (§4.4).
3. **Two index spaces, named.** `prev`/`next` are full-document row vectors;
   screen row = `anchor + (doc row − viewport_top)` (§4.1).
4. **Above-viewport changes are clamped out of the paint, but `prev` is still
   updated.** Scrollback is never cleared (§4.3).
5. **One trait, one registry, one caller-supplied footer.** No `_meta` registry
   (§7).
6. **`ToolKey`, not `ToolKind`, is the registry key** — the schema derives no
   `Hash` (§7).
7. **The wrap is ours**, over `Line::styled_graphemes()` + `unicode-width`;
   ratatui's `reflow` is private (§4.2).
8. **Mode 2026 is emitted unconditionally.** No capability detection (§4.4).
9. **One owner of stdin, raw mode for the session**, which means we own Ctrl-C
   and Ctrl-D and must supply a single-line editor (§13.1).
10. **`cost.rs` and `picker.rs` move to `apps/bitrouter` whole**, tests included
    (§8).

### C.1 Decisions required before Phase 2 starts

These are spec §19 open questions. Phase 1 does not depend on them; Phase 2 does.

- **§19.1 — is §4 accepted?** If no, this plan is void; the fallback is spec
  §17's first entry (defects 2 and 3 only) and needs its own much smaller plan.
- **§19.2 — line diff: dependency or hand-rolled?** **Answered 2026-08-14: a
  production dependency on `similar` (3.1).** The spec's own recommendation,
  and the reason stands — subtly wrong diff output is worse than a dependency.
  `similar` is the maintained standard (it is what `insta` diffs with), and it
  already groups changed runs into hunks with configurable context, which is
  precisely what §6 specifies; hand-rolling that is where the subtle wrongness
  would live. It is the only new *shipped* crate in this plan: `Cargo.lock`
  carries `diff 0.1.13` today, but only as a dev-only transitive of
  `pretty_assertions`. Task 2.4 adds it to the workspace manifest and to
  `bitrouter-tui`.
- **§19.5 — how does the app name ACP schema types?** Blocks task 4.1.

---

## D. Tasks

### Phase 1 — Prerequisites (app-side, no rendering change)

These change input and lifecycle behaviour only. They ship independently of the
renderer and are worth having even if §19.1 goes the other way.

- [x] **1.1 One owner of stdin, in raw mode, with a single-line editor**
  - Depends on: —
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: replace the `BufReader::lines` prompt reader (`:823`) and the two
    per-modal `EventStream`s (`:948`, `:1032`) with a single owner that holds
    raw mode for the session. Supply echo, backspace, word-delete, and
    bracketed paste. No history, no multi-line — that is spec §15 "later".
    Ctrl-C cancels a running turn and exits when idle; Ctrl-D ends the session
    when idle.
  - Done when: `rg -n 'BufReader::new\(tokio::io::stdin' apps/bitrouter/src`
    returns nothing, and exactly one `enable_raw_mode` call site remains.
  - Verify: `cargo nextest run -p bitrouter 2>&1 | tail -5`
  - Commit: `refactor(chat): one stdin owner in raw mode`

- [x] **1.2 Restore the terminal on all three exits**
  - Depends on: 1.1
  - Files: `apps/bitrouter/src/acp_cli.rs`, `apps/bitrouter/src/tui/lifecycle.rs`
  - Do: reuse the existing handle-free `restore()` (`lifecycle.rs:44`) and
    `install_panic_restore()` (`:56`), and register
    `ShutdownSignals::install()` (`tui/watch.rs:185`) as a `select!` arm.
    Extend `restore()` to disable raw mode if it does not already. Normal exit,
    panic, and INT/TERM/HUP must all land there.
  - Done when: a test or a documented manual check shows the terminal is out of
    raw mode after each of the three exits.
  - Verify: `cargo nextest run -p bitrouter 2>&1 | tail -5`
  - Commit: `fix(chat): restore the terminal on panic and signals`

- [x] **1.3 Refuse or degrade on a non-TTY**
  - Depends on: 1.1
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: gate `chat` on `std::io::stdout().is_terminal()`. When false, fall back
    to plain line output with no cursor control and no raw mode.
  - Done when: `bitrouter chat <agent> < /dev/null | cat` emits no escape
    sequences.
  - Verify: `cargo nextest run -p bitrouter 2>&1 | tail -5`
  - Commit: `fix(chat): degrade to plain output when not a tty`

- [x] **1.4 Phase 1 gate**
  - Depends on: 1.1, 1.2, 1.3
  - Do: run the three workspace checks and paste their output.
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15 && cargo clippy --all-features 2>&1 | tail -5 && cargo fmt -- --check`
  - Commit: — (no code change; tick only)

### Phase 2 — Additive crate work (nothing user-visible)

Everything here lands **alongside** the shipping `Transcript`/`Inline` stack.
The crate carries both until Phase 3 flips it. Tests stay green throughout, and
no user-visible behaviour changes.

- [x] **2.1 Wrapping: `unicode-width` and a row-wrap module**
  - Depends on: 1.4
  - Files: `Cargo.toml`, `crates/bitrouter-tui/Cargo.toml`,
    `crates/bitrouter-tui/src/wrap.rs`
  - Do: add `unicode-width` (already in `Cargo.lock` via ratatui — adds no new
    crate). Write `wrap(line: &Line, width: u16) -> Vec<Line<'static>>` over
    `Line::styled_graphemes()`, splitting spans at the break and carrying style.
  - Done when: CJK, emoji, and ASCII fixtures wrap to a row count equal to
    `Paragraph::new(line).wrap(Wrap{trim:false}).line_count(width)`.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -10`
  - Commit: `feat(tui): grapheme-aware row wrapping`

- [x] **2.2 The journal**
  - Depends on: 2.1
  - Files: `crates/bitrouter-tui/src/journal.rs`
  - Do: implement spec §3 in full — every field, `apply`, and
    `set_pending_permission`. Implement the sticky message-keying rule: a `None`
    chunk continues the open run; a differing `Some(id)` closes it.
  - Done when: all 8 `transcript.rs` tests have journal equivalents that pass,
    plus a sticky-keying test with interleaved `None`/`Some` chunks.
  - Verify: `cargo nextest run -p bitrouter-tui journal 2>&1 | tail -15`
  - Commit: `feat(tui): retained, id-addressed journal`

- [x] **2.3 `ToolKey`, `ToolRenderer`, and the registry**
  - Depends on: 2.2
  - Files: `crates/bitrouter-tui/src/render/mod.rs`
  - Do: spec §7 — the trait, `ToolContext`, the `ToolKey` mirror with
    `From<ToolKind>`, and the registry with a default renderer. Register the
    three or four v1 renderers. `ToolContext` carries no `expanded`,
    `raw_input`, or `locations`.
  - Done when: a registered `ToolKey` dispatches to its renderer and an
    unregistered one falls through to the default, both asserted.
  - Verify: `cargo nextest run -p bitrouter-tui render 2>&1 | tail -15`
  - Commit: `feat(tui): tool renderer registry keyed on ToolKey`

- [x] **2.4 Content and diff rendering**
  - Depends on: 2.3, and §C.1's §19.2 decision recorded
  - Files: `crates/bitrouter-tui/src/render/`, manifests if §19.2 adds a crate
  - Do: spec §6. `ToolCallContent::Content` renders and is capped at 40 rows
    with a `… N more lines` tail. Non-text `ContentBlock`s render a one-line
    description. Diffs render as hunks with three lines of context, a
    `… N unchanged lines` summary, a one-terminal-height cap, and **always the
    absolute file path**.
  - Done when: an `Execute` call's output appears and is capped; a one-line edit
    to a 500-line fixture produces a bounded row count naming the path.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -15`
  - Commit: `feat(tui): hunked diffs and capped tool output`

- [x] **2.5 The differential writer**
  - Depends on: 2.4
  - Files: `crates/bitrouter-tui/src/writer.rs`
  - Do: spec §4.1 in full — the two index spaces, the eight frame steps, the
    paint clamp, the wholesale `prev` update, the `set_cursor_position` before
    `append_lines` and after it, `clear_region(AfterCursor)` for trailing rows,
    the `SyncSink` seam emitting mode 2026 unconditionally, and full-redraw on
    width/height/shrink with `viewport_top` recomputed on width change.
    Never call `get_cursor_position` on the frame path.
  - Done when: §16.3's writer regressions pass against `TestBackend`, including
    **a change above `viewport_top` is not painted, `prev` is still updated, and
    the next frame still paints a below-fold change**.
  - Verify: `cargo nextest run -p bitrouter-tui writer 2>&1 | tail -20`
  - Commit: `feat(tui): differential writer over ratatui Backend`

- [x] **2.6 Scheduling and the per-entity render cache**
  - Depends on: 2.5
  - Files: `crates/bitrouter-tui/src/journal.rs`,
    `crates/bitrouter-tui/src/writer.rs`
  - Do: spec §5 — dirty flag, per-entity revision, `(width, revision)` cache,
    ~30 ms tick, the immediate list, and the preemption rule.
  - Done when: a test shows N chunk updates coalescing into one frame, and a
    permission rendering immediately.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -15`
  - Commit: `feat(tui): coalesced rendering with a per-entity cache`

- [x] **2.7 Phase 2 gate**
  - Depends on: 2.1–2.6
  - Do: run the three workspace checks. The shipping TUI must be **unchanged** —
    `Transcript` and `Inline` are still what the driver uses.
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15 && cargo clippy --all-features 2>&1 | tail -5 && cargo fmt -- --check`
  - Commit: — (tick only)

### Phase 3 — The cutover

**This is the one irreducibly big-bang step**, and it is a single commit rather
than a series: the driver can render through exactly one stack at a time. Every
part of the new stack is already built and tested by Phase 2, so 3.1 is a wiring
change, not new logic. If it fails review it reverts cleanly, because Phase 2
left the old stack intact.

- [x] **3.1 Flip the driver to the journal and the writer**
  - Depends on: 2.7
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: replace `Transcript` with `Journal` and `Inline` with the writer.
    Compose the footer (spec §4.5) from cost, route, mode, title and the pending
    permission — cost still via the crate's `cost.rs`, which moves in 4.1. Drop
    the turn-end `flush()`. Route `log_tail` to a direct stdout write after
    teardown (spec §10).
  - Done when: `bitrouter chat` renders a session with in-place tool-call
    updates, and `rg -n 'bitrouter_tui::(transcript|viewport)' apps/bitrouter/src`
    returns nothing.
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15`
  - Commit: `feat(chat): render through the journal and writer`

- [x] **3.2 Delete the superseded stack**
  - Depends on: 3.1
  - Files: `crates/bitrouter-tui/src/{viewport.rs,transcript.rs,lib.rs}`
  - Do: delete `viewport.rs` (and its 3 tests), `transcript.rs` (its 8 tests are
    already ported), and `Capabilities` with its 2 tests. Remove the `mod`
    declarations.
  - Done when: those paths do not exist and `rg -n 'Capabilities|LIVE_ROWS' crates/bitrouter-tui/src`
    returns nothing.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -10`
  - Commit: `refactor(tui): delete the append-only renderer`

- [x] **3.3 Phase 3 gate**
  - Depends on: 3.1, 3.2
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15 && cargo clippy --all-features 2>&1 | tail -5 && cargo fmt -- --check`
  - Commit: — (tick only)

### Phase 4 — Moves, new surfaces, and lockstep

- [ ] **4.1 Move `cost.rs` to the app, whole**
  - Depends on: 3.3, and §C.1's §19.5 decision recorded
  - Files: `crates/bitrouter-tui/src/cost.rs` → `apps/bitrouter/src/chat/cost.rs`
  - Do: move the constant, `Scope`, `from_usage`, `render`, `unreported`, and
    **all five tests**. The crate keeps nothing of it — `from_usage` reads the
    constant, so the split rev 2 proposed does not compile.
  - Done when: `crates/bitrouter-tui/src/cost.rs` does not exist and the five
    tests pass in their new home.
  - Verify: `cargo nextest run --all-features cost 2>&1 | tail -10`
  - Commit: `refactor(tui): move the cost surface to the app`

- [ ] **4.2 Move `picker.rs` to the app, whole**
  - Depends on: 4.1
  - Files: `crates/bitrouter-tui/src/picker.rs` → `apps/bitrouter/src/chat/picker.rs`
  - Do: move it as plain app code with **no registry entry** — it is not a
    `_meta` surface (spec §7). Its five tests move with it.
  - Done when: `crates/bitrouter-tui/src/picker.rs` does not exist.
  - Verify: `cargo nextest run --all-features picker 2>&1 | tail -10`
  - Commit: `refactor(tui): move the provider picker to the app`

- [ ] **4.3 Render the five forwarded variants**
  - Depends on: 3.3
  - Files: `crates/bitrouter-tui/src/journal.rs`, `crates/bitrouter-tui/src/render/`
  - Do: spec §9 — `AvailableCommandsUpdate`, `Plan`, `CurrentModeUpdate`,
    `ConfigOptionUpdate`, `SessionInfoUpdate`. Not `PlanUpdate`/`PlanRemoved`:
    they are behind `unstable_plan_operations`, which the workspace does not
    enable.
  - Done when: each of the five renders, asserted by name.
  - Verify: `cargo nextest run -p bitrouter-tui 2>&1 | tail -15`
  - Commit: `feat(tui): render plans, commands, modes and config`

- [ ] **4.4 Key bindings and turn cancellation**
  - Depends on: 4.3
  - Files: `apps/bitrouter/src/acp_cli.rs`
  - Do: wire spec §9's table. `Esc` is consumed by the innermost open modal and
    reaches turn-cancel only when none is open; cancelling with a permission
    outstanding **auto-denies** it via `Prompt::deny()`. `Ctrl-L` forces a full
    redraw.
  - Done when: `Session::cancel` has a caller, and a test shows a cancelled turn
    with an outstanding permission resolving to deny, never to consent.
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15`
  - Commit: `feat(chat): escape cancels a turn`

- [ ] **4.5 Make the boundary checks pass**
  - Depends on: 4.1, 4.2
  - Files: `crates/bitrouter-tui/src/log_tail.rs`
  - Do: normalize the one odd fixture path (`log_tail.rs:81`, `:91`) from
    `/home/u/.bitrouter/logs/session-1.log` to `/tmp/session-1.log`, matching
    the file's other three fixtures. Then run spec §16.5's checks.
  - Done when: `grep -rn "bitrouter/" crates/bitrouter-tui/src` returns nothing;
    `cargo tree -p bitrouter-tui` shows no `bitrouter`; `cost.rs`, `picker.rs`,
    and `viewport.rs` do not exist; and the app-side renderer module references
    no `Config`, metering, or control socket.
  - Verify: `grep -rn "bitrouter/" crates/bitrouter-tui/src && echo "FAIL: matches above" || echo "PASS: no matches"`
  - Commit: `test(tui): make the crate boundary check satisfiable`

- [ ] **4.6 Lockstep the docs**
  - Depends on: 4.4, 4.5
  - Files: `docs/CLI.md`, `skills/bitrouter/references/cli.md`,
    `docs/ACP_TUI_SPEC.md`, `docs/ACP_TUI_PLAN.md`
  - Do: spec §18 — the key-binding table; the note that `chat` holds raw mode
    for the session so enter/Ctrl-C/Ctrl-D are ours; §4.3's stale-row behaviour
    in the `chat` section; the §8 correction and status-line supersession in
    `ACP_TUI_SPEC.md`; a pointer from `ACP_TUI_PLAN.md` §C decision 7 and task
    4.5 to this spec.
  - Done when: all five documents name the bindings and none describes a
    `Viewport::Inline` status row as current.
  - Verify: `rg -n 'Ctrl-L|raw mode' docs/CLI.md skills/bitrouter/references/cli.md`
  - Commit: `docs: lockstep the chat renderer surface`

- [ ] **4.7 Phase 4 gate**
  - Depends on: 4.1–4.6
  - Verify: `cargo nextest run --all-features 2>&1 | tail -15 && cargo clippy --all-features 2>&1 | tail -5 && cargo fmt -- --check`
  - Commit: — (tick only)

---

## E. Goal strings

Paste one at a time, in order.

**Phase 1**

```
Work through Phase 1 of docs/TUI_RENDERER_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 1.1-1.4 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing. This phase touches input and lifecycle only — do not change any rendering code, do not touch crates/bitrouter-tui. Raw mode means we own Ctrl-C and Ctrl-D: implement them, and make sure every exit path (normal, panic, INT/TERM/HUP) restores the terminal. Never use #[allow], .unwrap, .expect or panic!. Print the full Phase 1 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 20 turns.
```

**Phase 2**

```
Work through Phase 2 of docs/TUI_RENDERER_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 2.1-2.7 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing. Everything in this phase is ADDITIVE: the shipping renderer must still be Transcript + Inline when the phase ends, and `bitrouter chat` must behave exactly as it does today. Do not modify apps/bitrouter/src/acp_cli.rs. Before task 2.4, confirm the §19.2 diff decision is recorded in §C.1 — if it is not, print BLOCKED and stop. Never use #[allow], .unwrap, .expect or panic!. Print the full Phase 2 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 40 turns.
```

**Phase 3**

```
Work through Phase 3 of docs/TUI_RENDERER_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 3.1-3.3 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing. Task 3.1 is the cutover and is a single commit — wiring only, no new rendering logic; everything it wires was built and tested in Phase 2. Do not delete anything until 3.1 is green, so the change reverts cleanly. Never use #[allow], .unwrap, .expect or panic!. Print the full Phase 3 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 20 turns.
```

**Phase 4**

```
Work through Phase 4 of docs/TUI_RENDERER_PLAN.md following its §A loop protocol: one task per turn, first unchecked task whose dependencies are met, tick its checkbox and commit. Done when tasks 4.1-4.7 are checked AND the final turn shows `cargo nextest run --all-features`, `cargo clippy --all-features`, and `cargo fmt -- --check` all passing, AND the transcript shows `grep -rn "bitrouter/" crates/bitrouter-tui/src` returning nothing. cost.rs and picker.rs move WHOLE to apps/bitrouter, tests included — do not split them. The picker gets no registry entry; it is not a _meta surface. Before task 4.1, confirm the §19.5 schema-types decision is recorded in §C.1 — if it is not, print BLOCKED and stop. A cancelled turn with an outstanding permission must resolve to deny, never to consent. Never use #[allow], .unwrap, .expect or panic!. Print the full Phase 4 checklist and a `PLAN STATUS:` line every turn, and paste each Verify command's real output — do not summarise it. Stop after 35 turns.
```

---

## F. Progress

Phase 1 ☑ · Phase 2 ☑ · Phase 3 ☑ · Phase 4 ☐

Tick a phase when its gate task passes. A phase whose goal cleared but whose
gate is unticked was **not** finished — re-read §D before trusting it.
