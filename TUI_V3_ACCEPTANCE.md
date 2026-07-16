# TUI v3 — Build & Acceptance Spec (loop-driving)

> **How to run.** Point a self-paced loop at this file:
> `/loop implement TUI v3 per @TUI_V3_ACCEPTANCE.md until Definition of Done`.
> The agent works one task per iteration, commits each green task, and **stops when
> §3 Definition of Done holds**.
>
> **Design source of truth:** [`TUI_SPEC_V3.md`](TUI_SPEC_V3.md) (the *why* + the invariants
> I1–I3 + the disposition table). **This doc is the executable checklist** (the *what* +
> the binary proof for each step). Read both every iteration; when they disagree, the
> design doc wins and you fix this doc.

---

## 0. Loop protocol — read this every iteration

**State is durable, not in your head.** The only things that survive between iterations are
**(a) the checkboxes in §2**, **(b) `git log`**, and **(c) the §4 Build Log**. Never assume
you remember prior work — re-derive your position from the ledger and git history at the
start of every iteration.

**Per-iteration algorithm:**
1. Run the **§1 standing gates**. If any is red, the previous iteration left the tree
   broken — fix that first, before anything else.
2. Find the **first unchecked `[ ]` task** in §2, in document order (V3.1 → V3.5).
3. Make the **smallest change** that satisfies that task, honoring the CLAUDE.md guardrails
   below and the pure-`reduce()` discipline (state pure; side effects returned as `Effect`s).
4. Run that task's **Proof** command(s) **and** the §1 standing gates.
5. **Only if all pass:** flip the box to `[x]`, append a one-line §4 Build Log entry
   (`<sha> — <task> — <proof that passed>`), and **commit** (conventional message, e.g.
   `refactor(tui): collapse Acp/Mirror panes into read-only Monitor`).
6. If something is red, fix it this iteration — do **not** check the box.
7. When **every box in §2 is `[x]`**, proceed to the **§3 Exit Gates** (they are also
   checkboxes; the same rules apply). When the §3 Definition of Done holds, **announce
   `DONE` and stop looping — do not start another iteration.**

**Guardrails (blocking, from `CLAUDE.md`):**
- **Never** `unwrap`/`expect`/`panic!`, `#[allow(...)]`, dead code, or public re-exports in
  `apps/bitrouter/src/tui/`.
- **Never** check a box you cannot prove with its listed command. A green `cargo test` for a
  test you didn't write is not proof — write the asserting test the task names.
- If a task is genuinely ambiguous, make the **smallest decision consistent with
  `TUI_SPEC_V3.md`**, record it in the Build Log as a `DECISION:` line, and keep moving.
  Do not stall the loop waiting for clarification.
- One task per iteration keeps commits reviewable and the loop resumable. Resist batching.

---

## 1. Standing gates — must be green before checking ANY box

Run from the worktree root. All four must pass:

```bash
cargo fmt -- --check
cargo clippy --all-features -- -D warnings
cargo nextest run --all-features   # or: cargo test --all-features
# src/tui purity (must print nothing):
grep -rnE '\.unwrap\(\)|\.expect\(|panic!|#\[allow' apps/bitrouter/src/tui/ \
  | grep -vE '#\[cfg\(test\)\]|mod tests' || true
```

The `grep` is an aid, not the authority — clippy + review are. But a hit there is a
guaranteed guardrail violation; clear it.

---

## 2. Phases & acceptance ledger

Each task is a checkbox with a concrete **Proof**. Phases are independent after V3.1;
do them in order. `apps/…` paths are under `apps/bitrouter/src/tui/` unless noted.

### V3.1 — Pane collapse (load-bearing; unblocks the rest)

- [x] **PaneKind has exactly `Pty` and `Monitor`.** `Acp` and `Mirror` are gone; all match
  arms updated.
  **Proof:** `grep -n 'enum PaneKind' -A6 state.rs` shows only `Pty`, `Monitor`; and
  `grep -rn 'PaneKind::Acp\|PaneKind::Mirror' apps/bitrouter/src` prints nothing.
- [x] **The composer is deleted.** Remove the `state.input` pane buffer, `render_input`, and
  the composer layout row ([`ui.rs:96`](apps/bitrouter/src/tui/ui.rs),
  [`ui.rs:128`](apps/bitrouter/src/tui/ui.rs)). (The palette keeps its own `palette.input` —
  that is not the composer.)
  **Proof:** `grep -rn 'fn render_input\|state\.input\b' apps/bitrouter/src/tui` prints
  nothing.
- [x] **A focused `Monitor` is read-only.** New reducer test `monitor_pane_is_read_only`:
  feeding `AppEvent::Key` chars then `Enter` to a focused `Monitor` returns no
  `Effect::Prompt`/`Effect::PtyPaste` and pushes no `Line::UserPrompt`.
  **Proof:** that test exists and passes.
- [x] **The composer never renders.** Render test (`TestBackend`) with a focused `Monitor`:
  no input-border row is drawn.
  **Proof:** that test exists and passes.
- [x] **BROADCAST is removed.** `Mode::Broadcast`, `broadcast_input`, `reduce_key_broadcast`,
  and the `Ctrl-B` handler are gone.
  **Proof:** `grep -rn 'Broadcast\|broadcast_input' apps/bitrouter/src/tui` prints nothing.
- [x] **Standing gates green** (§1).

### V3.2 — Command surface (one hub + a one-shot leader)

- [x] **`Mode::Agent` is removed;** `Mode` = `Normal · Leader · Command · Picker · Confirm`.
  **Proof:** `grep -n 'enum Mode' -A8 state.rs` shows exactly those five; no `reduce_key_agent`.
- [x] **The leader moves off `Ctrl-A`.** Default `Ctrl-Space`, read from a `tui.leader`
  config field (fall back to default when unset). `Ctrl-A` no longer enters any manager
  mode. Leader press opens a **one-shot** which-key overlay.
  **Proof:** reducer tests `ctrl_a_is_not_a_leader` (a focused `Pty` receiving `Ctrl-A`
  emits `Effect::PtyKey` passthrough, no mode change) and `leader_opens_whichkey` (the
  configured leader → `Mode::Leader` + overlay set).
- [x] **The leader is one-shot.** Every leaf key returns to `Normal` (or opens a
  `Command`/`Picker` leaf) in ≤1 key — never a sticky mode.
  **Proof:** reducer test `leader_leaves_are_one_shot` covering `1`,`Tab`,`n`,`p`,`c`,`a`,
  `t`,`?`,`Esc`.
- [x] **Leaf map wired** per `TUI_SPEC_V3.md` §3: `1..9` focus session · `Tab` next
  actionable · `n` new session (Picker/Session) · `p` palette · `c` close · `a` autonomy ·
  `t` attach · `?` keys.
  **Proof:** reducer tests asserting each leaf's `Effect`/mode transition.
- [ ] **Inline supervision unchanged.** From `Normal`: `y/a/n` resolve the top pending
  decision; `D/m/p/r` review the focused `Monitor`; `Ctrl-C` interrupts the focused agent;
  `PgUp/PgDn` scroll. Mouse click still focuses rows.
  **Proof:** existing decision/review reducer tests still pass (update, don't delete).
- [ ] **`n` is not a top-level spawn.** Direct human spawn exists only as the palette entry
  `spawn subagent` (`PickerPurpose::Subagent`); there is no `Normal`/leader path that spawns
  a subagent as a first-class key.
  **Proof:** `COMMANDS` contains `spawn subagent`; no reducer path maps a bare key to
  `PickerPurpose::Subagent`.
- [ ] **Standing gates green** (§1).

### V3.3 — Status bar (active pane | global fleet)

- [ ] **Left zone follows the focused pane:** context-window gauge (`used/size` or `%`) +
  model tag + pane cost, when the upstream reports them. Promote context occupancy out of
  the pane header ([`ui.rs:778`](apps/bitrouter/src/tui/ui.rs)). A transient notice still
  claims this zone and decays.
  **Proof:** render test asserting the left zone shows the focused pane's `ctx …%`/`used/size`,
  model, and `$cost`.
- [ ] **Right zone = global fleet:** badge counts (`⚠◆●◉`) + summed fleet cost + `serve ●/✗`.
  Fold the bare `N sessions` count.
  **Proof:** render test asserting the right zone contents and that `session` word is absent.
- [ ] **Verbose hint strings leave the bar.** The AGENT/NORMAL cheat-sheet lines
  ([`ui.rs:977`](apps/bitrouter/src/tui/ui.rs)) are replaced by a minimal persistent leader
  affordance (e.g. `⌃Space menu`); full hints live in the which-key overlay + palette.
  **Proof:** `grep -n 'AGENT  \[/\] panel' ui.rs` prints nothing; the long NORMAL hint is gone.
- [ ] **Standing gates green** (§1).

### V3.4 — Review routing by ownership

- [ ] **Panes carry an ownership marker** (orchestrator-owned vs. human-owned/hatch) on the
  pane or session record.
  **Proof:** the field exists and is set at spawn time for both paths.
- [ ] **Reject routes by ownership** per `TUI_SPEC_V3.md` §5: orchestrator-owned → an effect
  carrying the verdict as the subagent's task outcome (`changes_requested` + note), **not** a
  prompt; human-owned → a direct re-prompt effect.
  **Proof:** reducer tests `reject_orchestrator_owned_sets_task_outcome` and
  `reject_human_owned_reprompts`.
- [ ] **Standing gates green** (§1).

### V3.5 — Docs lockstep (mandatory — `CLAUDE.md` requires it)

- [ ] **Rewrite the `tui` rows in [`skills/bitrouter/references/cli.md`](skills/bitrouter/references/cli.md).**
  Remove every now-false claim: `Ctrl-A` manager leader, "composer renders where typing can
  land", AGENT + BROADCAST modes, `Ctrl-A N` new session, `Ctrl-B`. Describe: read-only
  `Monitor` panes (no composer), the one-shot `tui.leader` (default `Ctrl-Space`) + leaf map,
  the active-pane/global-fleet status bar.
  **Proof:** `grep -nE 'Ctrl-A → AGENT|Ctrl-B → BROADCAST|composer renders only where' skills/bitrouter/references/cli.md`
  prints nothing; a human-read pass confirms the new keys are described.
- [ ] **Update the `tui` section of [`CLI.md`](CLI.md)** to match (keys, no input bar,
  status bar).
  **Proof:** grep the same stale phrases in `CLI.md` → none.
- [ ] **Mark v2 superseded.** Add a one-line header note to [`TUI_SPEC.md`](TUI_SPEC.md)
  pointing to `TUI_SPEC_V3.md` for the changed surfaces (§3 command model, §8 panes, status
  bar), and fold any build-time deviations into the `TUI_SPEC_V3.md` §11 decision log.
  **Proof:** the note exists; the decision log reflects reality.
- [ ] **Standing gates green** (§1).

---

## 3. Exit gates — only after every box in §2 is `[x]`

### Gate A — full sweep from a clean tree
- [ ] `git status` is clean (all §2 work committed); re-run all **§1 standing gates**; all
  green. Record `BASE=<sha at loop start>` in the Build Log if not already there — it scopes
  the review + diff.

### Gate B — Fable-5 review (mandatory)
- [ ] **Launch a reviewer subagent on Fable-5.** Use the **Agent tool** with
  `subagent_type: general-purpose` (or `feature-dev:code-reviewer`), **`model: fable`**,
  `run_in_background: false`, and this prompt:

  > You are a Fable-5 reviewer. Review the full v3 diff:
  > `git diff <BASE>..HEAD -- apps/bitrouter/src/tui skills/bitrouter CLI.md TUI_SPEC.md`.
  > It implements TUI v3 per `TUI_SPEC_V3.md` + `TUI_V3_ACCEPTANCE.md`. Check, with
  > `file:line` and a **CONFIRMED/PLAUSIBLE** verdict per finding: (1) correctness &
  > regressions in the `reduce()` reducer and the render paths; (2) conformance to
  > `TUI_SPEC_V3.md` invariants **I1–I3** and the §7 disposition table (nothing that should
  > be deleted still reachable; nothing kept was dropped); (3) `CLAUDE.md` violations
  > (`unwrap`/`expect`/`panic!`/`#[allow]`/dead code/public re-export in `src/tui`); (4) the
  > docs lockstep is complete and accurate. Rank findings most-severe first.

- [ ] **Resolve every CONFIRMED finding.** For each: fix it, re-run §1 gates, and **re-run
  the Fable-5 reviewer** on the updated diff. Repeat until the reviewer returns **zero
  CONFIRMED** findings. Record each round's summary + resolutions in the Build Log.

### Gate C — live e2e (mandatory — actually drive the TUI)
Prefer the built-in **`/verify`** and **`/run`** skills to drive the app; the tmux recipe
below is the concrete fallback. Build first: `cargo build --features tui` (the `tui` feature
is default-on). Ensure `bitrouter serve` is running (else the TUI shows a `serve ✗` notice).

Drive `bitrouter tui` inside a PTY (tmux) and **capture evidence** to the scratchpad
(`tmux capture-pane -p > <scratchpad>/e2e-<n>.txt`). Use a configured **fake ACP agent** as
the `Monitor` (a scratchpad `bitrouter.yaml` with an `agents:` entry running a minimal
stdio-ACP responder — reconstruct via `/verify`, which knows this recipe), and, if auth is
available, a real `--agent claude` **orchestrator session** for the PTY path.

Assert, each with a captured snapshot:
- [ ] **Monitor is read-only.** `bitrouter tui --agent <fake>` renders it as a `Monitor`;
  typing text draws **no input bar** and creates **no prompt line** in its transcript.
- [ ] **Leader works; `Ctrl-A` doesn't manage.** The configured leader (`Ctrl-Space`) shows
  the which-key overlay; `Ctrl-A` does **not** enter a manager mode.
- [ ] **New session flow.** `leader n` opens the harness picker; selecting one adds an
  orchestrator session to the sessions rail (PTY pane takes keystrokes).
- [ ] **Status bar shape.** Left shows `ctx …% · <model> · $…` for the focused pane; right
  shows `⚠◆●◉ … · $<fleet> · serve ●`.
- [ ] **Inline decision + review.** A pending permission resolves with `y/a/n` from `Normal`;
  a finished subagent's diff loads with `D` and rejects/merges per §2 V3.4.
- [ ] Record evidence file paths + the key asserting lines in the Build Log.

### Definition of Done
- [ ] Every box in **§2** and **§3 (Gates A, B, C)** is `[x]`.
- [ ] Working tree clean; all changes committed on the branch.
- [ ] The Fable-5 reviewer's last run returned **zero CONFIRMED** findings.
- [ ] Live e2e evidence is recorded in §4.

When all of the above hold: **announce `DONE`, summarize the Build Log, and stop the loop.**
Do not schedule another iteration.

---

## 4. Build log (append-only — newest last)

Format: `<date> <sha> — <phase/task> — <proof / note>`. Add `DECISION:` / `BLOCKER:` lines
as needed. First iteration records `BASE=<sha>`.

```
BASE=b01c8887
DECISION: the §1 purity grep hits only test-gated modules (#[cfg(test)] /
  #[cfg(all(test, unix))]) at baseline — treated as green; clippy is the authority.
DECISION: the Acp-vs-Mirror behavior split is re-expressed as an Ownership
  { Human, Orchestrator } field on PaneState (set at spawn for both paths) —
  this front-fills V3.4's ownership-marker task, per TUI_SPEC_V3 §4/§5.
2026-07-15 a1f36fa7 — V3.1 PaneKind collapse — enum grep shows only Monitor/Pty;
  PaneKind::Acp|Mirror grep empty; fmt+clippy+1948 nextest green.
DECISION: the composer deletion also removes PaneState.draft + the draft
  stash/restore (write-never once the composer is gone = dead code); the
  substrate FleetAgent.draft field stays for format compat, always None.
DECISION: reject's feedback-as-next-prompt flow is gone with the composer;
  \`r\` now just clears review + notices "rejected" until V3.4 lands the
  ownership-routed verdict effects. Front-filled tests monitor_pane_is_read_only
  + composer_never_renders (the next two boxes) while porting broken tests.
2026-07-16 c3cca160 — V3.1 composer deleted — 'fn render_input|state.input' grep
  empty; fmt+clippy+1939 nextest green.
2026-07-16 148286cb — V3.1 read-only + never-renders boxes — front-filled tests
  monitor_pane_is_read_only (state.rs) + composer_never_renders (ui.rs) verified
  against their task specs; both pass via targeted nextest; gates green (clippy's
  sole warning = pre-existing proc-macro-error2 future-incompat dep note).
DECISION: BROADCAST removal also deletes PaneState.selected + the rail/pane ✓
  markers (selection existed only for broadcast) and Line::UserPrompt (nothing
  constructs it once composer+broadcast are gone — dead code; transcripts are
  now read-only by construction). monitor_pane_is_read_only tightened to
  lines.is_empty(). V3.4's human-owned re-prompt may reintroduce the variant.
2026-07-16 43d8de9e — V3.1 BROADCAST removed + phase gates box — proof grep
  'Broadcast|broadcast_input' empty in src/tui; fmt+clippy+1927 nextest green.
DECISION: dissolving AGENT also deletes Panel/rail_cursor/session_cursor/
  queue_only/rail_selected/panel_selected + Command::Queue (disposition: q is
  cut; the queue is always the rail head). Detail-slot focus switching is
  click-only; splits are palette-only and fill with the most actionable
  unshown agent. Rail ▸/key-chips follow the focused pane. Leader hardcodes
  DEFAULT_LEADER=Ctrl-Space this task; the tui.leader config field is the
  next task. Front-filled reduce_key_leader leaf map + leader_opens_whichkey
  and the Ctrl-A-passthrough assertions (task-2/3/4 tests build on these).
2026-07-16 cc869dbf — V3.2 Mode::Agent removed — enum grep shows exactly the
  five v3 modes; reduce_key_agent gone; fmt+clippy+1914 nextest green.
2026-07-16 efb29ebe — V3.2 leader off Ctrl-A — SDK TuiConfig{leader} (ctrl-<key>,
  fallback ctrl-space) + parse_leader + AppState.leader; tests
  ctrl_a_is_not_a_leader / leader_opens_whichkey / configured_leader_replaces_
  the_default pass; gates green (1917). NOTE: skills/bitrouter + CLI.md rewrite
  for the new config surface lands in V3.5 per this plan (lockstep deferred to
  that phase deliberately).
2026-07-16 0dfa39f6 — V3.2 leader one-shot — leader_leaves_are_one_shot
  covers 1/Tab/n/p/c/a/t/?/Esc, each ends in Normal|Picker|Command; gates
  green (1918).
2026-07-16 9c649224 — V3.2 leaf map wired — per-leaf reducer tests:
  leader_digit_focuses_session_n / leader_tab_focuses_the_next_actionable_agent /
  leader_n_opens_the_session_picker_and_enter_spawns /
  leader_p_opens_the_command_palette / leader_c_* (×3) /
  leader_a_cycles_autonomy_and_logs / leader_t_attaches_the_focused_live_monitor_only /
  leader_question_mark_opens_keys_help; 14 leader tests pass; gates green (1919).
```
