# BitRouter TUI — v3 Refactor Spec: Pure Control Tower

**Status:** Draft for review · **Supersedes:** the multiplexer residue in [`TUI_SPEC.md`](TUI_SPEC.md) (v2)
**Owner:** TUI · **Scope:** `apps/bitrouter/src/tui/` — unreleased surface, so these are **clean deletes, no back-compat burden**

---

## 0. Why v3

v2 shipped the control-tower *fabric* — PTY orchestrator pane, ACP subagent spine, fleet
MCP bridge, review queue, worktree isolation. But it kept v1's **multiplexer machinery**
bolted on beside it: a tmux-style `Ctrl-A` manager mode, a **human-promptable** subagent
pane, and a broadcast fan-out. Three symptoms follow directly:

1. **The command surface is too complex.** One leader (`Ctrl-A`) gates a sticky AGENT mode
   whose hint line is thirteen verb-groups ([`ui.rs:985`](apps/bitrouter/src/tui/ui.rs)),
   and `Ctrl-A` steals readline `Home` from the orchestrator — the one pane the human lives
   in (v2 §9 concedes this wart).
2. **Sessions vs. subagents are conflated.** A subagent pane (`PaneKind::Acp`) gets a
   composer and the human can type prompts straight into it
   ([`state.rs:1816`](apps/bitrouter/src/tui/state.rs)) — even though v2's model says only
   the *orchestrator* steers subagents (`PaneKind::Mirror` is already read-only,
   [`state.rs:1783`](apps/bitrouter/src/tui/state.rs)). Two pane kinds, opposite rules.
3. **The status bar carries hints, not the numbers you watch.** Context-window occupancy is
   buried in the pane header ([`ui.rs:778`](apps/bitrouter/src/tui/ui.rs)); the bar's left
   zone spends its width on a mode cheat-sheet.

**Root cause: one unfinished turn.** v2 §1's thesis is *"the human supervises one
conversation (the orchestrator) and a decision queue"* — but the UI still lets the human
multiplex N subagent panes. v3 finishes the turn by **deleting the multiplexer half**. The
human's only input surface is the orchestrator conversation(s); subagents are read-only
monitors the orchestrator drives; the human **decides and reviews**, nothing else.

---

## 1. Thesis, sharpened — three invariants

- **I1 — One interactive surface class.** Only orchestrator **sessions** (native harness
  TUIs on a PTY) take human input. Everything else is a monitor.
- **I2 — Subagents are never typed into by the human.** A subagent pane is a **read-only
  transcript** plus its decision + review affordances. Free-text steering is the
  orchestrator's job (MCP tools, v2 §4).
- **I3 — The human is never *in* a manager mode.** Supervision is **inline** (resolve a
  decision, review a diff, right where it surfaces) plus a **one-shot leader** for the few
  rare verbs. No sticky mode to enter and `Esc` out of.

Everything below is these three made literal.

---

## 2. Pane model — collapse three kinds to two

```
BEFORE — 3 kinds                          AFTER — 2 kinds

  Pty ─────────────────────────────────▶  Pty        orchestrator SESSION
      orchestrator, interactive,               │      interactive — the ONLY
      keys → child                             │      surface you type into
                                               │
  Acp  ┐  human-spawned, HAS composer,         │
       │  human prompts it  ◀── the mismatch   │
       ├──── minus composer / prompt ───────▶  Monitor    subagent, read-only:
       │                                                  transcript + decision
  Mirror┘ orch-spawned, read-only                         queue + review
```

- **`Pty`** (orchestrator session) — unchanged. Native harness TUI, locked-mode
  passthrough, the only interactive pane.
- **`Monitor`** = old `Acp` ∪ `Mirror`, always **read-only**. bitrouter renders it from
  typed ACP events (two-region streaming, `diff_render`, syntect — all v2 §8b, kept). It
  carries the decision queue (§5) and review verbs (§5) but **no composer**.

**Deleted with the collapse:**
- the bitrouter composer / input bar (`state.input`, `render_input`, the `Acp`-focus branch
  of the composer condition at [`ui.rs:96`](apps/bitrouter/src/tui/ui.rs));
- the `Enter → Effect::Prompt(subagent)` path ([`state.rs:1816`](apps/bitrouter/src/tui/state.rs));
- **BROADCAST** entirely (`Mode::Broadcast`, `broadcast_input`, `Ctrl-B`) — fan-out to
  subagents is the orchestrator's job;
- human-spawn as a *first-class* verb (`n`) — demoted to a palette hatch (§4).

**Attach escape hatch preserved.** The fidelity tier (v2 §3) stays: a `Monitor` can be
*temporarily attached* as a PTY (`t`) to drive one agent at full fidelity; on detach it
returns to read-only `Monitor`. This is the deliberate, explicit exception to I2 — driving
one agent, not routine steering.

**Code delta.** `PaneKind::{Acp, Mirror}` → `PaneKind::Monitor`; delete `state.input` +
`render_input` + the composer layout row; delete `Mode::Broadcast` + `broadcast_input` +
`reduce_key_broadcast`; the `Acp`-vs-`Mirror` reducer branches fold to one read-only path.

---

## 3. Command surface — one hub, a one-shot leader

```
BEFORE  ·  6 modes, TWO hubs (NORMAL + AGENT), dense edges
           the Ctrl-A edge steals readline Home from the orchestrator

                ┌────────────────────────────────────────────┐
                │                  NORMAL                     │  default
                └────────────────────────────────────────────┘
                   │ Ctrl-A           │ Ctrl-B        │ : (empty)
                   ▼                  ▼               ▼
       ┌────────────────────────┐ ┌───────────┐ ┌───────────┐
       │         AGENT          │ │ BROADCAST │ │  COMMAND  │
       │ [/] j/k Enter s/v q    │ └───────────┘ └───────────┘
       │ y/a/d D/m/p/r t A x    │
       │  n │ N   (spawn/new)   │
       └────┼───┼───────────────┘
            ▼   ▼
       ┌───────────┐    ┌───────────┐
       │  PICKER   │──▶ │  CONFIRM  │
       └───────────┘    └───────────┘


AFTER  ·  one hub (NORMAL); leader is ONE-SHOT, not a sticky mode
          AGENT + BROADCAST deleted; PICKER/CONFIRM survive as leaves

   inline in NORMAL (no mode change):
     y/a/n    resolve top pending decision (batch-clears → next)
     D/m/p/r  review focused Monitor (diff / merge / apply / reject)
     Ctrl-C   interrupt focused agent        PgUp/PgDn  scroll
     click any rail row → focus

                     ┌───────────────────────────┐
     ┌──<leader>────▶│           NORMAL           │◀── run / Esc ──┐
     │  one-shot,    │  PTY passthrough  |  read-  │                │
     │  which-key    │  only transcript           │                │
     │               └───────────────────────────┘                │
     ▼                              │ p                            │
  leader leaves                     ▼  palette-launched leaves     │
  ┌───────────────┐          ┌───────────┐  ┌──────────┐  ┌──────────┐
  │ 1..9 session  │          │  COMMAND  │─▶│  PICKER  │  │ CONFIRM  │
  │ Tab next act. │          │ (palette) │  │ harness  │  │ bootstrap│
  │ n new · c clo │          └───────────┘  └──────────┘  └──────────┘
  │ a tier·t att. │           all rare verbs  new session /  first
  │ ? keys · Esc  │                           spawn hatch    isolated spawn
  └───────────────┘
```

**The shape change is the argument.** AGENT stops being a second hub. v3:

- **Deletes `Mode::Agent` (sticky manager mode) and `Mode::Broadcast`.**
- **Leader becomes a one-shot prefix**, not a sticky mode. Press it → a **which-key
  overlay** (the existing `keys_help` mechanism) → one leaf key → back in NORMAL. You are
  never "in manager mode." Leaf map (≤8, mostly navigation):

  | leader + | action |
  |---|---|
  | `1`..`9` | focus session N (switch orchestrator conversation) |
  | `Tab` | focus next actionable subagent (needs-you → review) |
  | `n` | new session (harness picker) |
  | `p` | open the command palette (exhaustive rare verbs) |
  | `c` / `a` / `t` | close · autonomy tier · attach — on the focused pane |
  | `?` / `Esc` | keys help · cancel |

- **Leader default = `Ctrl-Space`**, configurable via `tui.leader`, disambiguated by the
  kitty keyboard protocol bitrouter already negotiates. Rationale: `Ctrl-A` is readline
  `Home` in the primary surface — the worst possible choice. Exact byte is **spike-gated**
  across the v2 §11 terminal matrix (see §11).
- **Inline, no leader, no mode:** pending-decision `y/a/n` (resolves the top item, advances
  to the next — batch clear); `D/m/p/r` review on the focused `Monitor`; `Ctrl-C` interrupt;
  `PgUp/PgDn` scroll; **mouse click** focuses any rail/session row (already shipped).
- **Palette (`Mode::Command`) is the one exhaustive surface** for rare verbs; **`Picker` and
  `Confirm` survive only as leaves** reached from the palette / a spawn flow, never
  top-level.

**Mode enum after:** `Normal · Leader · Command · Picker · Confirm` (was `Normal · Agent ·
Picker · Broadcast · Command · Confirm`). The count barely moves; the **character** changes —
the two *sticky hubs* become *one hub + a one-shot prefix*, and BROADCAST is gone.

---

## 4. Sessions vs. subagents — the ownership rule, made literal

- **New session (kept, first-class).** The human opens an orchestrator conversation and
  **picks the harness** at creation (`Picker`, `PickerPurpose::Session` — already exists,
  [`state.rs:108`](apps/bitrouter/src/tui/state.rs)). Reached via `leader n` or the sessions
  rail `+`. Multiple sessions switch via `leader 1..9` / clicking the rail.
- **Subagents are orchestrator-owned.** Only orchestrators spawn and steer them, through the
  MCP fleet tools (v2 §4). The human sees a read-only `Monitor`.
- **Thin human-spawn hatch (palette-only).** The one exception: `spawn subagent` stays in
  the command palette for a **human-owned** background task with no orchestrator behind it.
  It is *not* a first-class key and *not* the mainline. It exists because it cleanly
  justifies the ownership-split review routing in §5 — and for the "just run this in the
  background" moment that doesn't warrant narrating to an orchestrator.

---

## 5. Decisions & review — kept, routing clarified

The decision queue is v1/v2's strongest surface and is **unchanged in spirit**; v3 only
removes the *mode* around it.

- **Decision queue.** A pending ACP permission surfaces at the rail head; `y/a/n` resolves it
  inline from NORMAL; decisions across N subagents **batch** into one risk-sorted pass
  (v2 §5). No mode entry.
- **Review verbs** on a focused `Monitor`: `diff` / `merge` / `apply` / `reject`. Writes stay
  **human-gated by default** (v2 §5) — unchanged.
- **Reject routing splits by ownership** (this is the resolved fork):
  - **Orchestrator-owned subagent** → the human's verdict becomes the subagent's **MCP task
    outcome** (`changes_requested` + note). The orchestrator consumes it (blocking-with-
    summary today; `tasks/input_required` when harnesses adopt MCP Tasks — v2 §4/§5). **No
    injection into the orchestrator PTY**, no direct re-prompt.
  - **Human-owned (hatch) subagent** → `reject` re-prompts it directly (the old
    feedback-as-next-prompt loop) — the human *is* the owner, so direct steering is correct
    here and only here.

---

## 6. Status bar — "active pane | global fleet"

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ ctx 62% · claude-opus-4-8 · $0.41        ⚠1 ◆1 ◉2 · $3.80 · serve ●          │
│ └────────── focused pane ──────────┘     └──────────── global fleet ─────────┘│
└─────────────────────────────────────────────────────────────────────────────┘
   << and >> sidebar-toggle buttons stay at the bar's edges
```

- **Left zone follows the focused pane** (the active conversation, or the subagent you're
  reviewing): **context-window gauge** (`used/size` or `%`) + **model** + **pane cost**.
  This promotes context occupancy out of the pane header ([`ui.rs:778`](apps/bitrouter/src/tui/ui.rs))
  to the thing you actually watch — orchestrators auto-compact, and the gauge tells you when
  to wrap a thread. A **transient notice** still claims this zone and decays (kept).
- **Right zone stays global fleet:** `⚠◆●◉` decision counts · **fleet cost** · `serve ●/✗`.
  Fold the bare `N sessions` (the sessions rail already shows them). *Burn rate ($/hr)* is a
  better-than-cumulative signal but needs cost deltas over time — **deferred** (keep the
  honest cumulative number now).
- **Verbose hints leave the bar.** The mode cheat-sheet moves to the **which-key overlay**
  (on leader) + the palette. The bar becomes a **gauge, not a cheat-sheet** — keep only a
  minimal persistent affordance (`⌃Space menu`). Killing the modal manager (§3) is what
  frees this width.
- **Edge `<<` / `>>` sidebar toggles kept** ([`ui.rs:1032`](apps/bitrouter/src/tui/ui.rs)).

---

## 7. Disposition table — every current verb

`✂` delete · `→` move · `●` keep. This is the sheet to red-line.

| Today (v1/v2 multiplexer) | Fate | Where it lands in v3 |
|---|---|---|
| `Ctrl-A` → AGENT mode | ✂/→ | leader moves off `Ctrl-A`; AGENT mode dissolved into inline + leader |
| `Ctrl-B` / BROADCAST | ✂ | orchestrator's job |
| `Enter` in ACP pane → prompt subagent | ✂ | subagent is read-only |
| `n` — human spawns subagent | → | palette hatch only (§4), not first-class |
| `N` — new orchestrator session | ● | `leader n` / sessions-rail `+` (harness picker) |
| `s` / `v` / unsplit — detail splits | ✂ | one focused detail pane; monitors are the rail. (Keep in palette only if a real need appears.) |
| `[` `]` `j` `k` `Enter` — rail nav | → | click row (no mode); `leader Tab` / `leader 1..9` for keyboard |
| `q` — collapse rail to queue | ✂ | the decision queue is always the rail head |
| `y` / `a` / `n` — resolve permission | ● | inline, no mode |
| `D` / `m` / `p` / `r` — review verbs | ● | inline on the focused `Monitor`; reject routes by ownership (§5) |
| `t` — attach subagent as PTY | ● | `leader t` — the fidelity escape hatch (§2) |
| `A` — autonomy tier | ● | `leader a` / palette |
| `x` — close | ● | `leader c` / palette |
| `:` — command palette | ● | now reached via `leader p` (can't type `:` into a passthrough PTY) |
| CONFIRM — bootstrap gate | ● | unchanged (palette/spawn-flow leaf) |
| PICKER — harness choice | ● | unchanged (palette/new-session leaf) |

---

## 8. Phasing — each slice independently shippable

- **V3.1 — pane collapse (load-bearing).** `PaneKind::{Acp,Mirror}` → `Monitor`; delete the
  composer, `Enter→Prompt`, BROADCAST. *Exit:* no human input reaches a subagent; the
  manager view is a read-only transcript; `state.input` is gone.
- **V3.2 — command surface.** Delete `Mode::Agent`; add the one-shot `Leader` prefix +
  which-key map; palette becomes the rare-verb hub; move the leader off `Ctrl-A`. *Exit:* no
  sticky manager mode; every supervision action reachable by keyboard *and* mouse.
- **V3.3 — status bar.** Re-layout to active-pane-left / global-right; promote the context
  gauge; move hints to which-key. *Exit:* context %, model, and cost are always visible for
  the focused pane.
- **V3.4 — review routing.** Reject-by-ownership (orchestrator task outcome vs. direct
  re-prompt). *Exit:* rejecting an orchestrator-owned subagent surfaces in the orchestrator's
  context, never the PTY.

V3.1 unblocks the rest; V3.2–V3.4 can land in any order after it.

---

## 9. Standing gates (v2 §14 carried) + v3 deltas

All of v2 §14 (mechanical + polish rubric + PTY fidelity) still applies. New for v3:

- **No sticky mode but NORMAL.** The leader is one-shot: any leaf returns to NORMAL in ≤1
  key (reducer test).
- **No input widget while a `Monitor` or `Pty` is focused.** The composer is gone; assert it
  never renders (render test).
- **Keyboard parity.** Every supervision action (switch session, next actionable, resolve
  decision, review, close, autonomy, attach) is reachable with no mouse (which-key + palette
  coverage test).
- **Leader doesn't collide.** Under the negotiated kitty protocol, the leader key never
  reaches the orchestrator child; the fidelity spike (§11) confirms it across the matrix.

---

## 10. Deleted from v2 (net simplification)

- `Mode::Agent`, `Mode::Broadcast` — the two extra hubs.
- `PaneKind::Acp`, `PaneKind::Mirror` — merged into `Monitor`.
- `state.input` composer + `render_input` + the composer layout row.
- `broadcast_input` + `reduce_key_broadcast` + `Ctrl-B`.
- the `Enter → Prompt(subagent)` reducer path; the human-facing subagent prompt entirely.
- the verbose mode-hint strings in the status bar (moved to which-key).
- `n` as a first-class spawn key; `q` queue-toggle; (candidate) `s`/`v`/unsplit.

Retained: the pure `reduce()` + `Effect`s, the two rails (sessions/subagents) + radar +
actionable head, autonomy tiers, review/merge queue, worktree isolation, attention beacons,
durable fleet memory — all of v2's fabric.

---

## 11. Resolved decisions (v3 log)

Resolved with the recommended defaults; don't reopen without reason.

1. **Leader = `Ctrl-Space`, configurable (`tui.leader`), one-shot prefix — not `Ctrl-A`, not
   a sticky mode.** Rationale: `Ctrl-A`/`Ctrl-B` are readline; the primary surface is a
   passthrough PTY. Exact byte spike-gated across the v2 §11 matrix
   (`Ctrl-]` / `Ctrl-\` are fallback candidates).
2. **Human-spawn hatch = kept, palette-only, not first-class.** Covers the human-owned
   background task and anchors the §5 review split.
3. **Reject routing = by ownership.** Orchestrator-owned → MCP task outcome the orchestrator
   consumes; human-owned → direct re-prompt. No PTY injection.
4. **Status bar left = follows the focused pane** (context / model / cost); right = global
   fleet. Verbose hints move to which-key.
5. **`Picker` / `Confirm` survive as palette/flow leaves; `Agent` / `Broadcast` deleted.**
   The claim is "two hubs → one," not "6 modes → 3."

**Build-time deviations (v3 as shipped — the decisions the build added):**

6. **Ownership is an explicit field.** The `Acp`-vs-`Mirror` behavior split became
   `PaneState.owner: Ownership { Human, Orchestrator }`, set at spawn for both paths —
   capability edges (cancel / attach / autonomy / close / fleet membership) and §5's
   reject routing key off it, not the pane kind.
7. **Composer plumbing went with the composer.** `PaneState.draft` + the draft
   stash/restore are deleted (write-never = dead code); the durable `FleetAgent.draft`
   wire field stays for format compatibility and is always `None`. `Line::UserPrompt`
   is deleted too — monitor transcripts are read-only **by construction**. Pane
   `selected` marks (`✓`) died with BROADCAST.
8. **The manager cursor machinery is gone entirely.** `Panel`, rail/session cursors,
   queue-only mode, and the palette `queue` command are deleted; the queue is always
   the rail head. Detail-slot focus switching is click-only; `s`/`v` splits survive as
   palette commands only, filling with the most actionable unshown agent (resolving
   §11.5's open question: splits stay, palette-only).
9. **Reject carries a canned note.** With no composer there is nothing to type: reject
   sends a fixed `changes_requested` note. Orchestrator-owned → `Effect::ReviewVerdict`
   → `TuiMsg::ReviewVerdict` over the fleet socket → `subagent_status` reports
   `state: changes_requested` + `review_verdict` (the task outcome, per §5);
   human-owned → a direct re-prompt with the same note.
10. **Batch clear shipped.** `y/a/n` resolve the top pending decision (roster head —
    risk-sorted, oldest first) regardless of focus, and focus advances to the next
    pending item.
11. **Leader spec-as-shipped.** `tui.leader` is a `ctrl-<key>` string (default
    `ctrl-space`), matched as `Char(<key>)+CONTROL` under the negotiated kitty
    protocol; the exact-byte terminal-matrix spike remains for Gate C / follow-up.
    The which-key overlay renders whenever the leader prefix is armed; `:` still
    opens the palette from a focused `Monitor` (alongside `leader p`).

**Still open (smaller):** the leader byte across the full v2 §11 terminal matrix
(spike); burn-rate metering for the status bar.
