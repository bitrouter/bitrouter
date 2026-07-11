# BitRouter TUI — Design Spec & Acceptance

Target design + checkable exit criteria for `bitrouter tui`, the in-process
multi-agent manager (`apps/bitrouter/src/tui/`). Doubles as the goal for the
refinement loop: the loop works the phases in order and keeps the standing gates
green. The TUI is **production-ready** when Phases 0–4 pass every box and the
standing gates (mechanical + polish) hold. Phases 5–6 are v2.

## North star

A multi-agent manager is **not a terminal multiplexer** — it's a **control
tower**. tmux/zellij assume the human drives and the machine waits; here N
agents drive and the human waits, so the scarce resource is **your attention**.
The whole UI routes attention to the agent that needs you and keeps the rest as
calm blips. Confirmation fatigue is a real security risk (you stop reading
prompts you mash `y` on), so we interrupt only for what's consequential.

## Design principles (settle future arguments with these)

1. **One screen.** A fixed left rail + a splittable detail viewport. No separate
   "views" to switch between, no tabs, no grid mode.
2. **Overview → detail.** The rail is the canonical list of every agent; you
   drill into 1–N of them in the detail split on demand.
3. **Route attention, don't broadcast it.** Actionable agents surface at the top
   of the rail with inline resolve — not N modals.
4. **Interrupt only for the consequential.** Risk-tiered autonomy decides what
   reaches you; everything else auto-resolves and is logged (never silent).
5. **One uniform renderer.** Every harness renders through the manager's own line
   types, distinguished only by a terse tag (`claude-code`, `codex`). No
   per-harness theming, no native-TUI passthrough (see Rendering model).
6. **Polish is a gate, not a nicety.** The polish rubric is re-verified every
   iteration alongside the mechanical gates.
7. **Degrade, never crash.** Small terminals, dead agents, missing `serve` — all
   produce a legible state, not a panic.

## The one screen

```
┌ roster · 6 agents ──┐┌ api-1 · claude-code ┬ ui-3 · codex ─────── ┐
│▸● api-1  ⚠ needs you ││ › consolidating the │ › restyling the tab │
│ ● test-2 ⛔ blocked   ││   guard checks…     │   bar with flexbox… │
│ ○ ui-3   ⣷ running   ││ ⚙ edit auth/mod.rs  │ ⚙ edit tui/ui.rs    │
│ ○ docs-4 ⣷ running   ││ ⚠ wants: rm -rf     │                     │
│ ✓ perf-5 ✓ review    ││   legacy/ [y/n/a] _ │ ›_                  │
│ ✗ pi-6   ✗ died      ││                     │                     │
├──────────────────────┤│                     │                     │
│ radar ▇▁▅▃▁▁         ││                     │                     │
├──────────────────────┤│                     │                     │
│ ⚠ 2 need you         ││                     │                     │
│ ▸ api-1  rm -rf …    ││                     │                     │
│   test-2 net GET …   ││                     │                     │
└──────────────────────┘└─────────────────────┴─────────────────────┘
  left rail (fixed)        detail: splittable viewport (default 1)
  Ctrl-A mgmt · : cmd · Ctrl-B broadcast · q focus-actionable · ↵ open
```

**Left rail (fixed):** three stacked sections over one flat agent list —
1. **Roster** — every agent, one row (agent · task · state · Δdiff · $ · activity
   sparkline), sorted `needs-you > blocked > review > running > idle > dead`,
   stable within a bucket.
2. **Radar** — a thin per-agent state strip; a background agent flipping to
   needs-you updates here without you leaving the detail.
3. **Actionable head** — the agents that need you, expandable inline to their
   pending action + risk + `y/n/a`. This *is* the decision queue; it's the top of
   the same list, not a separate widget. `q` collapses the rail to only this.

**Detail (splittable viewport):** shows 1 agent by default; opt-in split
horizontally/vertically to watch 2–4 at once. The split is ephemeral layout
state, not a persistent structure. This single mechanism replaces both the old
tiled grid and tabs.

## Rendering model

All harnesses run as **ACP servers over stdio** (confirmed: no PTY anywhere in
the tree; `up.rs` `spawn_process` → `initialize` + `session/new`). The manager is
the sole renderer: it folds ACP `SessionUpdateKind` into its own `Line` types and
draws them uniformly, tagged by harness. Consequences, by deliberate choice:

- **No per-harness theming** — a terse `agent · harness` header is the only
  distinction; styling is identical across harnesses.
- **No native-TUI passthrough** — "render the harness's own TUI" is impossible
  over ACP (ACP is structured JSON-RPC, not a terminal transport), and the PTY
  alternative (claude-squad model) would forfeit the structured signals that
  power the rail. Native-TUI-on-attach is parked in v2 as a deliberate escape
  hatch (Phase 6), never the default.

## Architecture impact (grounded in current code)

Headline: the state model flattens. Tabs and the grid go away.

```
// before
AppState { tabs: Vec<Tab { title, panes: Vec<PaneState>, focus }>, active_tab, … }
// after
AppState { agents: Vec<PaneState>, detail: DetailLayout, rail_focus, … }
//   DetailLayout { shown: Vec<AgentIdx>, split: Split }  // which agents, how split
```

| Piece | Change | TUI-only? |
|---|---|---|
| Roster + radar | Project the flat `agents` list, sorted; reuse `PaneState.attention` for the sort | ✅ |
| Splittable detail | New `DetailLayout`; replaces `tabs` **and** the grid render | ✅ |
| Actionable head / queue | Projection over each pane's existing `PaneState.pending` (`AppEvent::Permission` is already typed; resolve via existing `Effect::ResolvePermission`) | ✅ |
| Tiered autonomy | New `AutonomyLevel` per pane + deterministic `classify_risk`; below threshold → auto-`ResolvePermission`, else surface | ✅ |
| Command palette | New `Mode::Command` + fuzzy match over a command table | ✅ |
| Harness tag | Derived from the agent's configured harness — already known | ✅ |
| Review queue | Needs a typed "turn complete/idle" `SessionUpdateKind` + diff/commit effects | ❌ substrate |
| Attach (native TUI) | PTY lifecycle + interactive relaunch/`session/load` | ❌ substrate, v2 |

Input model stays modal and minimal: **Normal** (keys → focused detail pane's
prompt) · **Agent** `Ctrl-A` (rail nav, split, spawn, close, autonomy) ·
**Picker** (spawn selection) · **Broadcast** `Ctrl-B` · **Command** `:`.

## Standing gates (re-verify EVERY iteration — must never regress)

### Mechanical
- [ ] `cargo nextest run --all-features` passes (fallback `cargo test`)
- [ ] `cargo clippy --all-features` — zero warnings
- [ ] `cargo fmt -- --check` clean
- [ ] No panicking `unwrap()`/`expect(`/`panic!` in `src/tui/`; no `#[allow]`;
      no dead code; no public re-exports

### Polish rubric (apply to whatever the current iteration drew)
- [ ] **Alignment:** columns align to the character; numbers right-aligned;
      unicode-width-correct truncation — CJK/emoji never drift columns; overflow
      ends in `…`, never a hard cut or ugly wrap; box junctions clean
- [ ] **Color:** small semantic palette (needs-you/danger · running/attention ·
      done/ok · idle-dead/dim); **never color-alone** (glyph + color); legible in
      dark *and* light; honors `NO_COLOR`; degrades on 16-color/dumb terminals
- [ ] **Liveness:** braille spinner on running rows; **frame coalescing** so a
      chatty agent doesn't repaint per-token (no flicker, no CPU spin); keypress
      response <~100ms; subtle flash on a row that just changed
- [ ] **Edge states:** 0- and 1-agent rail look intentional; pre-first-output
      detail shows a calm `thinking…`; dead agent is clear but not alarming;
      empty actionable head reads "all clear"; no debug/raw text leaks
- [ ] **Responsive:** narrow width collapses the rail and drops columns in
      priority order (sparkline → cost → task) rather than wrapping; sane
      min-size fallback; resize is artifact-free
- [ ] **Focus & input:** focused pane/row unmistakable but not garish; prompt
      cursor visible; backspace/paste/scroll smooth with a scrollback position
      indicator; the hint line always matches the live keybindings

## Loop protocol

Per iteration: pick the top unchecked item in the lowest open phase → make the
smallest change that closes it → verify **behaviorally** (drive the TUI or a
reducer test modelling the real key sequence) → check the box with an evidence
note → if a flag/key/behavior changed, update `skills/bitrouter/references/cli.md`
(TUI section) the same iteration → re-run the standing gates → commit
(conventional message). Stop and ask if an item needs a product decision not in
this doc, or if two iterations make no checkbox progress.

## Phase 0 — Foundations (build once)

- [x] Terminal restored (raw mode off, alt-screen left) even on a panic in the
      event loop — verified via a panic-hook/fault-injection test
      — `install_panic_restore` chains restore before the default hook;
      test `tui::tests::panic_hook_restores_terminal_before_reporting`
- [x] Resize down to a tiny terminal (e.g. 20×5) degrades without panic/artifacts
      — test `tui::ui::tests::tiny_terminals_render_every_surface_without_panic`
      renders all surfaces (grid/popups/notice/zoom) at 1×1…80×1; resize
      redraw path documented in the event loop
- [x] Focused detail pane scrolls its scrollback (PageUp/Down) with an off-tail
      indicator; new output while scrolled up does not yank to bottom
      — content-pinned `PaneState.scroll` + `⇣N` marker; tests
      `pageup_pins_view_and_new_output_does_not_move_it`,
      `pagedown_returns_to_follow_at_tail`, `scroll_pin_tracks_ring_buffer_drain`,
      `pinned_pane_shows_off_tail_indicator_and_history`
- [x] `bitrouter serve` not running → actionable error, not a hang
      — startup TCP probe of `cfg.server.listen` sets a warning notice
      (`probe_serve_*` tests); prompt failures surface as red `✗` pane lines
      instead of vanishing into tracing (`prompt_failed_*` tests)
- [x] `Ctrl-C` tears down all sessions cleanly from every mode
      — hoisted to a global check in `reduce()` (was swallowed by
      Agent/Picker/Broadcast fallthroughs, which also ate the loop's
      synthesized quit on stream end); test `ctrl_c_quits_from_every_mode`;
      teardown runs via the loop's `cleanup()` on every exit path

## Phase 1 — One screen: rail + splittable detail

- [ ] State model flattened to `agents: Vec<PaneState>` + `DetailLayout`; `tabs`
      and the grid render deleted
- [ ] Left rail shows roster (sorted by actionability) + radar; is the default
      landing surface
- [ ] Detail viewport shows 1 agent by default; `Ctrl-A` split H/V shows 2–4;
      un-split returns to 1
- [ ] `↵` on a roster row opens that agent in the detail; rail focus + detail
      focus are both unmistakable
- [ ] Radar reflects a background agent's state change within one frame
- [ ] Terse `agent · harness` header on each detail pane; uniform styling

## Phase 2 — Actionable head / decision queue (permissions)

- [ ] Agents needing you surface at the top of the rail, ordered by risk then age
- [ ] An actionable row expands inline to its pending action + risk + `y/n/a`;
      resolves via `ResolvePermission`
- [ ] The rail head and the pane-inline prompt are two surfaces of the **same**
      pending — resolving one clears the other (no double-resolve, no orphan)
- [ ] `q` collapses the rail to actionable-only (focus mode); `Esc` restores
- [ ] `Ctrl-C` / a dying agent removes its queued item cleanly

## Phase 3 — Tiered autonomy

- [ ] Per-agent level `Manual | Assisted | Auto`, default `Manual`; changed with
      `a` from the rail; shown on the row
- [ ] `classify_risk` is deterministic and unit-tested (write-in-worktree=low;
      write-outside / delete / network / spend>threshold=high)
- [ ] `Assisted` auto-allows low-risk & surfaces high-risk; `Auto` auto-allows
      all; `Manual` surfaces all
- [ ] Every auto-resolved decision is logged into the pane scrollback

## Phase 4 — Command palette + which-key

- [ ] `:` opens a fuzzy palette over a command table (spawn, close, split,
      broadcast, diff, autonomy, kill-done, quit); `Enter` runs, `Esc` cancels
- [ ] A leader key shows a which-key menu of actions for the current mode
- [ ] Palette/which-key don't panic on empty or single-item states

## Phase 5 — Review queue (v2 — substrate work)

- [ ] Substrate emits a typed "turn complete/idle" signal (not inferred)
- [ ] Finished agents surface in the rail head for review; `d` shows the diff
- [ ] Approve → commit+push effect; reject → feedback returned as next prompt

## Phase 6 — Attach + voice/ambient (v2, optional, flagged)

- [ ] **Attach** (`A`): hand the whole terminal to an interactive PTY run of one
      harness (authentic native TUI) until a detach key pops back; manager pauses
      structured supervision of only that agent while attached
- [ ] **Voice:** agent-needs-you speaks a one-liner; voice reply routes to that
      agent via `rotate vox`

## Resolved decisions (log — don't reopen without reason)

- 3 views → **one screen** (rail + splittable detail); **tabs and grid deleted**.
- Queue = the roster's **actionable head**, not a separate widget; `q` focus mode.
- Permissions shown **both** inline and in the head — same pending, one source.
- New-agent autonomy default = **Manual** (fatigue managed by batching, not by
  defaulting to auto).
- Cost = a **`$` column** in the roster now; full cost HUD deferred.
- Harness distinction = **terse tag only**; no per-harness theming.
- Native harness TUI = **Attach escape hatch in v2**, never default; not "via ACP".

## Non-goals / deferred

Detach/reattach of the *manager* (needs daemon ownership; TUI is in-process) ·
session persistence across restarts · full cost-HUD dashboard · mouse beyond
what ratatui gives free · config hot-reload. Agent **grouping**, if wanted later,
returns as roster **filters/tags**, not as tab containers.
