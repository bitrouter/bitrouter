# BitRouter TUI — Composite Manager Design Spec (v2)

**Status:** Draft for review · **Supersedes:** [`TUI_ACCEPTANCE.md`](TUI_ACCEPTANCE.md) (v1, deprecated)
**Owner:** TUI · **Depends on:** `crates/bitrouter-substrate` (ACP, PR #613), harness catalog (`apps/bitrouter/src/harness.rs`, PR #705)

---

## 0. Why a new spec

v1 (`TUI_ACCEPTANCE.md`) shipped a real control-tower manager: a pure Elm reducer,
a rail sorted by actionability, risk-tiered autonomy, and a strong polish bar. Keep
all of that. But v1 made one structural assumption that this spec overturns:

> **v1 assumed bitrouter is the sole renderer** — every agent runs as an ACP server
> and the manager folds ACP updates into its own `Line` types and draws them
> uniformly. "No native-TUI passthrough" was a stated non-goal.

That forfeits the one thing users actually want from their daily driver: **the real
Claude Code / Codex TUI, at full fidelity.** It also put bitrouter in the business of
re-implementing a chat renderer it will always lose to the harness.

**v2 inverts the model.** The human's primary agent (the *orchestrator*) runs in its
**own native TUI, wrapped in a PTY pane** that bitrouter composites its control-tower
chrome around. bitrouter stops competing with the harness UI and becomes the **fabric**:
a global view, an MCP control surface, and the ACP substrate underneath. This is the
correct expression of "we work with *any* harness, any model, any workflow" — and of
the control-tower thesis, which v2 keeps and realizes more faithfully.

The spine is **ACP structure**. Subagents speak ACP, so their status, permissions, and
usage are *typed events* — the decision queue is real, not screen-scraped. That
structured layer is the moat over pure-terminal managers (herdr, rmux-as-product), which
can only infer state.

### What changed, in one table

| Dimension | v1 (deprecated) | v2 (this spec) |
|---|---|---|
| Orchestrator UI | bitrouter re-renders it from ACP | **Native harness TUI in a PTY pane** |
| bitrouter's role | the renderer | **the fabric**: view + MCP + ACP substrate |
| Who orchestrates | the human, typing into N panes | **the orchestrator agent** (MCP tools) + human supervises |
| Subagent transport | ACP (uniform render) | ACP (spine) — rendered from events *or* attached as PTY |
| Rendering core | ratatui line types only | ratatui chrome + **wezterm-term PTY panes** |
| Decision queue | over ACP permissions (kept) | over ACP permissions (kept, now the primary human loop) |
| Isolation | initial agent only | **worktree-per-subagent** (fixes a correctness bug) |
| Review/merge | deferred (Phase 5) | **first-class**, driven by ACP `stop_reason` |
| Session ownership | in-process `Vec<Arc<Session>>` | **daemon-owned fleet**; TUI is a renderer/host client |

---

## 1. Thesis (unchanged, realized correctly)

A multi-agent manager is a **control tower**, not a terminal multiplexer. N agents
drive; the human waits. The scarce resource is the human's **attention**, and the UI's
job is to route it to the agent that needs a consequential decision and keep the rest as
calm blips. Confirmation fatigue is a security risk.

v2 realizes this by splitting the work:

- **The orchestrator agent** does the orchestrating — it decides to spawn subagents,
  delegates, and collects results, via MCP tools it calls from its native TUI.
- **The human** makes only the *consequential* calls — approve a high-risk subagent
  action, review and merge a finished branch — in bitrouter's **global view** wrapped
  around the orchestrator pane.
- **Policy** auto-resolves everything reversible and worktree-confined, logged never
  silent.

The human is not typing prompts into N panes (v1's latent multiplexer). The human
supervises one conversation (the orchestrator) and a decision queue.

---

## 2. Architecture & process topology

Three concerns, cleanly separated. The **daemon owns the fleet**; the **TUI renders and
hosts**; the **MCP bridge** lets the orchestrator drive.

```
┌───────────────────────────── bitrouter tui (outer process) ─────────────────────────┐
│  owns the outer terminal (alt-screen, ratatui)                                        │
│                                                                                       │
│  ┌─ left rail ────────┐┌─ orchestrator pane (PTY) ────────────────────────────────┐  │
│  │ roster · radar     ││  claude-code / codex — its REAL native TUI                │  │
│  │ ▸⚠ needs-you queue  ││  (wezterm-term core; the human's primary surface)         │  │
│  │ 🔴🟡🔵🟢 status      ││                                                            │  │
│  └────────────────────┘└───────────────────────────────────────────────────────────┘  │
│         ▲ ACP events (subagent status/permissions/usage)   │ MCP tool calls           │
└─────────┼──────────────────────────────────────────────────┼──────────────────────────┘
          │                                                   │
   ┌──────┴───────────────── bitrouter fleet daemon (per-repo) ─┴──────────────────────┐
   │  ACP substrate: owns warm subagent sessions, worktrees, permissions, transcript │
   │  fleet registry ── record_id ↔ session ↔ worktree ↔ MCP task                    │
   │                                                                                  │
   │   ├─ ACP subagent: codex   (worktree, routed via serve)                          │
   │   ├─ ACP subagent: gemini  (worktree, routed via serve)                          │
   │   └─ ACP subagent: claude  (worktree, routed via serve)                          │
   └──────────────────────────────────────────────────────────────────────────────────┘
             the LLM proxy (`bitrouter serve`) stays a separate concern (routes HTTP)
```

**Ownership rule (the decision that settled wezterm-term over rmux):** there is exactly
**one session owner — the substrate/daemon.** The TUI is a *renderer + PTY host*, not a
session owner; wezterm-term is a *rendering core* fed by the TUI, not a competing
multiplexer. This keeps the substrate (with its warm-session + `acp attach` machinery,
already built) as the single source of truth for the fleet.

**Data flow.**
- The orchestrator calls an MCP tool (`spawn`, `send`, `status`, `diff`, `merge`).
- The **MCP bridge** — a stdio `bitrouter mcp serve` subprocess running a new `fleet`
  backend (a thin client of the fleet daemon), whose config the manager injects into the
  orchestrator when it launches it — translates the call into a fleet operation. **stdio,
  not HTTP:** fleet tools *mutate* (spawn processes, write your repo), and the shipped MCP
  HTTP→local mode is unauthenticated behind a loopback check; stdio inherits your process
  identity for free. An HTTP mount on the fleet daemon is a later, additive option for
  remote/GUI orchestrators, gated on a local-auth story.
- The daemon launches/steers an **ACP subagent** (worktree-isolated), and streams typed
  updates back.
- The **TUI** subscribes to the daemon's fleet stream and renders the rail + decision
  queue from those typed events. It also hosts the orchestrator PTY pane.

**Process boundary (resolved, §15-Q3).** The fleet lives in its **own per-repo daemon**
(`bitrouter fleet serve`, auto-started like `serve`, control socket at
`<repo>/.bitrouter/fleet.sock`). `bitrouter serve` stays purely the LLM proxy: it is a
global singleton that restarts routinely (`update --restart`, config reload) and holds
provider keys, none of which should touch — or be able to kill — N warm agent children.

**This reversal is real substrate work, not "assembling primitives."** The substrate is
per-process today: one session per process, a hardcoded `pid: std::process::id()` in the
record, and warm-session/`acp attach` serving exactly one session per socket. A fleet host
needs new machinery — a session **registry**, a **multiplexed fleet event stream** for the
TUI, record-schema changes (daemon pid vs. child pid), and **orphan/crash recovery** (stale
`Running` records). Size it as substrate work in §7, not as glue.

**Availability is asymmetric — name it.** Daemon ownership means *subagents* survive the TUI
closing (a v1 non-goal, now nearly free). The **orchestrator does not**: it is a PTY child
of the TUI process and dies with it. v2 accepts this — orchestrator continuity is the
harness's own `--resume`/session files, not bitrouter's job. (Daemon-holding the
orchestrator PTY and re-rendering it on reattach is rmux-grade work we declined in §11.)

---

## 3. The screen (PTY-composite)

One screen: a fixed **left rail** (the control tower) beside a **detail region** that is
**one or more PTY/ACP panes**, with an input line and mode bar. This replaces v1's
"splittable detail of re-rendered agents" with "the orchestrator's native TUI + optional
extra panes."

```
┌ roster · 4 ─────────┐┌ orchestrator · claude-code ──────────────────────────────────┐
│▸🔴 api-1  needs you  ││  (native Claude Code TUI, full fidelity, PTY-hosted)          │
│  └ high · rm -rf …   ││   › consolidate the guard checks                              │
│ 🟡 ui-3   working    ││   ⚒ Edit auth/mod.rs                                          │
│ 🔵 test-2 review     ││   …                                                           │
│ 🟢 docs-4 idle       ││                                                               │
├─────────────────────┤│                                                               │
│ radar 🔴🟡🔵🟢       ││                                                               │
├─────────────────────┤│                                                               │
│ ⚠ 1 needs you        ││                                                               │
│ ▸ api-1  rm -rf …    ││                                                               │
│   [y]es [a]lways [d] ││                                                               │
└─────────────────────┘└───────────────────────────────────────────────────────────────┘
 NORMAL  ‹leader› manage · keys route to the orchestrator pane · : cmd
```

**Fidelity tiering (the key UX idea).** Fidelity scales with stakes:

- **Composited pane** (beside the rail) = a *monitor*: reduced width, graphics scoped
  off, "good enough to glance at." An ACP subagent's monitor pane is rendered from
  events; the orchestrator's is its live PTY.
- **Fullscreen / attached pane** (rail hidden, one agent) = *near-transparent PTY
  passthrough*: full width, full caps, native feel — for when you're **driving** one
  agent. `attach` on an ACP subagent relaunches it interactively in a PTY for authentic
  native fidelity, then pops back to structured supervision on detach.

The hard case (pixel-perfect compositing) is thus the *low-stakes* case (monitoring), and
the high-stakes case (driving one agent) is the *easy* case (fullscreen passthrough).

**Detail pane sources:**
| Pane kind | Rendered by | When |
|---|---|---|
| Orchestrator | PTY (wezterm-term), native TUI | always — the primary surface |
| ACP subagent — monitor | bitrouter, from ACP events (§8) | default glanceable view |
| ACP subagent — attached | PTY (wezterm-term), native TUI | on `attach`, to drive it |
| Non-ACP agent | PTY (wezterm-term) + inferred status (§10) | the "any CLI agent" fallback |

---

## 4. Control plane: ACP spine + MCP spawn/manage

**Subagents are ACP (the spine).** Every managed subagent is a `bitrouter spawn <agent>`
ACP session (PR #705): worktree-isolated, routed through the daemon, emitting typed
`SessionUpdateKind` + permission callbacks + `stop_reason`. This is what makes the
decision queue, risk-tiering, and review gates *real* rather than inferred.

**The orchestrator drives via MCP tools.** A new MCP tool namespace (the deferred "MCP
spawn tools" from `SPAWN_SPEC.md`) exposes the fleet to the orchestrator harness:

| Tool | Effect |
|---|---|
| `spawn(agent, task, worktree?)` | launch an ACP subagent on an isolated worktree; returns a handle |
| `send(handle, text)` | prompt a running subagent |
| `status(handle?)` | fleet or per-agent state snapshot |
| `diff(handle)` | the subagent's branch diff vs. base |
| `apply(handle)` / `merge(handle)` | integrate its work (§7) |
| `close(handle)` | tear down (worktree retained until merged/discarded, §6) |

(Reconcile these names with SPAWN_SPEC §11's direction — `spawn_subagent`/`prompt_session`/
`session_status` — in one pass; the roles are the same.)

**Model the contract on MCP Tasks, capability-gated.** MCP's Tasks extension
(`working / input_required / completed / failed / cancelled`, with `input_required`
carrying elicitations resolved via `tasks/update`) is *exactly* the async + human-in-loop
shape a long-running subagent needs. Adopt its **lifecycle as bitrouter's internal
subagent state machine now**, regardless of transport:

- If the orchestrator declares `io.modelcontextprotocol/tasks` → return a task handle;
  `input_required` can route a subagent's approval **back into the orchestrator
  conversation**.
- If not (today's reality — no shipping harness consumes Tasks yet) → fall back to
  **blocking-with-summary** (`spawn` blocks and returns a summary, Claude-Code-Task-tool
  style), and subagent approvals resolve in **the global view** (§5).

Because the internal model is Task-shaped either way, adopting the wire protocol later is
a capability flag, not a rewrite.

**The skill.** A `skills/bitrouter/` orchestration skill teaches the harness the workflow
(when to spawn vs. do it inline, how to phrase subagent tasks with clear boundaries and an
output contract, depth-1 delegation — subagents don't spawn subagents). Per repo policy,
any CLI/flag/tool change updates the skill in lockstep.

**Result contract (resolved, §15-Q5).** Subagent results should be **machine-consumable** —
model on goose's `response.json_schema`. Ship it **with B2** as an *optional* per-spawn
contract: a `result_schema` parameter on the `spawn` tool and `--result-schema` on
`bitrouter spawn -p`. The change is additive — the NDJSON terminal line is already typed, so
it gains a `"result": {…}|null` field and the schema text rides the subagent's prompt; the
substrate needs nothing. On invalid output: **one repair re-prompt, then `schema_ok:false` +
raw text** so the orchestrator is never blocked. Bare `spawn -p` stays byte-compatible.

---

## 5. Decision & attention model

This is v1's strongest surface, kept and upgraded. The decision queue is the **actionable
head of the rail** — needs-you agents surface at the top, expandable inline to their
pending action + risk + resolve keys. It is fed by **real ACP permission events**, so it
knows *what* the agent wants and *how risky* it is (not just "a pane is blocked").

**Escalation homes (both, one state machine):**
- **v1 default → the global view.** A subagent's gated permission surfaces in the rail's
  decision queue; the human resolves it there. The orchestrator's `collect`/`status`
  reports `blocked_on_human`.
- **Forward-compat → the orchestrator conversation.** When harnesses adopt MCP Tasks
  `input_required`, the same pending can route back to the orchestrator to reason about
  or relay to the human.

**Risk model — upgrade from binary to reversibility + trust-boundary.** v1's
`classify_risk` is `Low/High` over ACP `ToolKind` + path. Upgrade to Claude Code
auto-mode's axes:
- **Reversible + in-worktree ⇒ auto** (git is the undo).
- **Irreversible / crosses a trust boundary / bypasses review ⇒ gate.**
- **Read-only fast-path** never consults the classifier (latency/cost).

**Autonomy tiers** (`Manual | Assisted | Auto`, per agent) stay. Add:
- **`AllowThread`** — ACP's fourth permission verb (allow for the rest of *this* session),
  the natural "stop asking me about this agent's edits" tier. v1 maps only
  AllowOnce/AllowAlways/Deny.
- **A circuit breaker** — N-consecutive (or total) auto-allowed-then-reverted / denied
  actions escalates the agent back to `Manual` and pings the human. An `Auto` agent must
  not silently auto-allow into a wall forever.

**Batch the inbox across agents.** With N subagents, gated decisions collect into **one
queue sorted by risk tier**, cleared in a pass — not N interruptions.

**Arbitration & write-gating (design work, not wiring).** `Session::permissions()` is a
*single-consumer* stream today (first caller wins), so the fleet daemon must **fan a pending
out** to both the view and (later) the orchestrator, and define who-may-resolve,
double-resolve idempotency, and where `AllowAlways`/`AllowThread` policy persists
(daemon-side, so it outlives the TUI). And **writes are human-gated by default**: `apply`
and `merge` (§7) integrate into your base repo and therefore *bypass review*, so by the
trust-boundary rule they gate to the human. The orchestrator may **request** a merge; it
may **perform** one only under an explicit autonomy tier you granted — never by default.

---

## 6. Isolation (worktree-per-subagent)

**Fixes a real correctness bug.** Today spawned agents launch with
`LaunchOptions::default()` → `worktree: None` (`apps/bitrouter/src/tui/mod.rs`), so N
subagents share the base repo working directory and clobber each other's uncommitted
files — the exact failure every parallel-agent tool exists to prevent (Claude Code issue
#55724). **Every subagent gets its own worktree + branch, by default.**

**Migration (the default is a *flip*).** `spawn` is opt-in `--worktree` today, and
`Session::open()` lets the manager's `cwd` win when no worktree is set — so the flip is
**scoped to fleet-managed subagents only**; bare `bitrouter spawn -p`/`--serve` keep
opt-in `--worktree` (byte-compatible for the GUI/shell). Define branch naming
(`bitrouter/<agent>-<record16>`) and base-ref (the manager's HEAD) explicitly.

Three costs, solved as first-class concerns (not afterthoughts):
- **Bootstrap hook.** Worktrees exclude untracked files (`.env`, `node_modules`). A
  per-worktree bootstrap step (copy/symlink/install) runs on creation. It **executes shell
  on worktree creation** — treat it as a code-execution surface: config-declared, shown to
  the human on first use, and gated under the same autonomy discipline as agent permissions.
- **Per-agent port allocation.** Assign each subagent a `$PORT` from a pool (uzi's trick)
  so N dev servers don't collide; surface the URL in the roster row.
- **Cleanup gated on merged-or-discarded.** Never auto-delete a worktree on close — that
  loses work (#55724). Retain until the branch is merged or the human explicitly discards.

Container-per-agent is a deferred opt-in tier (Sculptor model) for untrusted work; not v2.

---

## 7. Review & integration

The deferred v1 Phase 5, now first-class — and **less blocked than v1 claimed.** The
"turn complete/idle" signal it needs already flows: `Session::prompt()` returns a
`PromptResponse` with a typed `stop_reason` (`crates/bitrouter-substrate/src/engine.rs`).
v1's TUI discards it (`apps/bitrouter/src/tui/mod.rs`). Capture it.

**Review queue.** A subagent whose turn ends `EndTurn` with a non-empty diff surfaces in
the rail head as **ready to review**. Writes are **human-gated by default** (§5) — the
orchestrator may *request* them but performs them only under a granted autonomy tier:
- `diff` — the branch diff, rendered with the Codex `diff_render` treatment (§8).
- `apply` — drop changes into the working tree **uncommitted** (human writes the commit).
- `merge` — merge the branch **keeping history**.
- **reject → feedback-as-next-prompt** — the rejection note becomes the subagent's next
  prompt (the inline-comment → reprompt loop).

**Serialized integration (merge queue semantics).** Never merge N branches optimistically.
Integrate one at a time against `base + already-merged`, re-running checks — the fix for
"green in isolation, broken together." Model on GitHub's merge-queue.

**Per-worker verification gates.** Model on goose's `retry.checks`: a subagent can carry
shell success-checks (`cargo test`, `cargo clippy`) that must pass before it is "ready to
review." A failing gate loops back to the subagent, not the human.

**Substrate work required (the genuine v2 dependency):** the fleet layer itself — session
**registry**, a **multiplexed event stream**, **permission fan-out** (§5), record-schema
changes and **orphan recovery** (§2) — plus git diff/commit/merge effects and the
branch/worktree ↔ review-item mapping. The *signals* (`stop_reason`, usage/cost) already
exist; the fleet plumbing does not.

---

## 8. Rendering model

Two rendering paths, by pane kind:

### 8a. PTY panes (orchestrator, attached subagents, non-ACP agents)
Rendered by the **`TerminalBackend` trait**, default impl = **wezterm-term** (§11). The
backend owns VT parsing, the cell grid, scrollback, images, hyperlinks, **and input
encoding**. bitrouter feeds PTY bytes and draws the cell grid into the pane `Rect`. No
bitrouter line-rendering here — the harness renders itself.

### 8b. ACP-event panes (subagent monitors)
Rendered by bitrouter from typed `SessionUpdateKind` into `Line` types — v1's model, but
fixed to Codex-grade. **A0 (the first work item) targets exactly this path:**

- **Two-region streaming (fixes the core defect).** Today every `MessageChunk` becomes its
  own scrollback `Line` (`apps/bitrouter/src/tui/state.rs`) — streamed deltas render as
  one word per line. Adopt Codex's model: a **stable region** (committed) + a **mutable
  tail**, committing **only newline-terminated text**, so a half-formed line never flashes.
  Loop-level frame coalescing (already present) handles repaint; this handles *content*.
- **Syntax highlighting** for code blocks (syntect + two_face; drop italic/underline; cap
  by size).
- **Diff rendering** (`diff_render`): full-width `Line.style()` bg tint, syntax over tint,
  dimmed deletions, `⋮` between hunks, `+N/-M` chips. Replaces the current
  `format!(" [{status:?}]")` tool lines and the uncolored diff popup.
- **Usage/cost.** `SessionUpdateKind::Usage` already carries `cost: Option<UsageCost>`
  (`crates/bitrouter-substrate/src/translate.rs`); the TUI currently drops it. Surface a
  `$` roster column + context-occupancy in the pane header.

### 8c. Reusable Codex mechanics (both paths)
`key_hint.rs` (cross-terminal key matching), `HistoryCell` compact-vs-full trait (maps onto
collapsed-rail-row vs. expanded-log), `FrameRequester` coalescing at a 120fps clamp, and
`motion.rs`-style reduced-motion discipline enforced by a lint test.

---

## 9. Terminal fidelity mechanics (PTY panes)

The three caveats of PTY-compositing, each with a proven mechanism (herdr is the reference
implementation; **AGPL — reimplement, don't copy**):

- **Input routing — zellij "locked mode," not a heavy prefix.** When a pane is focused,
  bitrouter intercepts **exactly one leader key**; everything else passes through
  untouched — including **`Ctrl-C` → interrupt the focused agent** (NOT quit; a change
  from v1 where Ctrl-C quits globally) and `Ctrl-A` (readline). Put the leader in the
  **Kitty-keyboard keyspace** (enabled on the outer terminal via crossterm) so it cannot
  collide with any legacy binding; fall back to a zellij-style toggle when unavailable.
- **Input encoding — offload to the backend.** wezterm-term encodes key/mouse → the
  inner PTY's expected bytes, tracking the inner app's kbd mode (kitty flags; `Ctrl+J` →
  raw `\n` unless `REPORT_ALL_KEYS`). This is the herdr #106 class of bug; owning it is
  exactly why we pay for a full core over vt100.
- **OSC passthrough — the tmux `allow-passthrough` pattern.** A byte-level splitter peels
  side-effect sequences (OSC-52 clipboard, OSC-8 hyperlinks, title, notifications) out of
  the PTY stream and re-emits them **verbatim to the outer terminal** (herdr's
  `Osc52Forwarder`, ~50 lines, 256 KiB cap), while screen content goes to the backend.
- **Capability scoping.** Answer the inner PTY's Device-Attributes/terminfo queries with a
  conservative, honest set (truecolor/bracketed-paste/SGR-mouse/OSC-52 yes; **graphics off
  for composited panes**), so the inner app never emits what we can't render. Full caps on
  fullscreen/attached panes.
- **Resize recovery.** On pane resize: backend resize → SIGWINCH the child → re-probe the
  bottom detection region (§10) so status inference survives reflow; debounce during drag.

---

## 10. Status model (unified `AgentState`, two sources)

One `AgentState { Idle, Working, Blocked, Review, Dead }` drives the rail, fed by two
adapters — this is the edge over pure-terminal managers:

- **ACP agents → structural, exact.** Permission pending = Blocked; turn active = Working;
  `stop_reason` + non-empty diff = Review; else Idle. Free and accurate.
- **Non-ACP / opaque PTY agents → herdr's inference (the fallback).** Process-name →
  agent identity → a **per-agent manifest rule engine** (prioritized `contains`/`regex`
  rules over the terminal tail + OSC title/progress) → raw state; **PTY activity is the
  Working authority**; a **hysteresis state machine** debounces (Working→Idle needs N
  confirmations over a window unless a visible idle marker; Blocked publishes immediately
  and re-asserts). Manifests are data, hot-updatable, only needed for the non-ACP set.

The rail renders one status column; the human never needs to know which adapter produced
it.

---

## 11. Backend & dependency decision

**`TerminalBackend` trait, default = `wezterm-term`.** Rationale (decided in review):

- ACP structure is the spine, so bitrouter needs a **rendering core under its existing
  session-owning substrate** — not a competing multiplexer. wezterm-term owns nothing; the
  substrate stays the single session owner.
- wezterm-term includes **input encoding** (CSI-u/kitty) — the hardest, most bug-prone
  piece — plus images + hyperlinks. `alacritty_terminal` omits input encoding (you'd
  hand-write the herdr #106 bug) — rejected. `rmux` was rejected because its daemon
  duplicates and competes with the substrate's session ownership, and its snapshot-over-IPC
  widget renders least directly on the orchestrator pane (the one we most want crisp).
- **In-process, single-process** — fits "the default UX in the default binary."
- PTY spawning via `portable-pty` (expect to vendor/patch, as herdr did).

**Escape hatch:** the trait keeps `alacritty_terminal` / **libghostty-FFI** (herdr's pick)
available if cross-terminal fidelity proves decisive. **Validation gate:** before
committing, a spike must A/B a live `claude-code` + `codex` pane on our target terminal
matrix (`{Ghostty, iTerm2, kitty, Terminal.app, WezTerm, tmux, Windows Terminal}`) — the
Shift-Enter-under-kitty class of bug is the thing to probe. **Windows caveat:** PTY-composite
via ConPTY is in scope for B3, but warm-session/`acp attach` are Unix-only today, so
*TUI-detach* on Windows is deferred with the rest of detach.

---

## 12. Deprecated from v1

- **"No native-TUI passthrough."** Reversed — native PTY panes are the core of v2.
- **Splittable detail of re-rendered agents.** The orchestrator is a native PTY pane;
  extra panes are monitors or attaches, not a symmetric multiplexer of re-rendered agents.
- **In-process `Vec<Arc<Session>>` ownership.** The fleet moves to the daemon.
- **"Cost deferred for lack of metering data."** Cost already flows via
  `UsageCost`; it was being dropped.
- **"Review queue blocked on a new substrate signal."** `stop_reason` already flows; the
  real dependency is git integration, not the signal.
- **Ctrl-C = global quit.** In PTY panes, Ctrl-C interrupts the focused agent; quit moves
  to the leader.

Retained from v1: the pure `reduce()` reducer + `Effect`s, the rail/roster/radar/actionable
head, autonomy tiers, and **all standing gates** (§14).

---

## 13. Phasing

Sequenced **A0 → B → polish**, remapped to this architecture. **Target scale (§15-Q4):**
1 orchestrator + ~2–6 ACP subagents; no opaque-native panes in v1.

**A0 — make ACP output reviewable (TUI-only, unblocks everything).**
- Two-region streaming + syntect + `diff_render` for ACP-event panes (§8b).
- Capture `stop_reason`; surface `UsageCost`.
- *Exit:* a subagent's output and diff are legible enough to review.

**B1 — isolation (TUI + engine).**
- Worktree-per-subagent by default; bootstrap hook; port pool; cleanup-gated (§6).
- *Exit:* N subagents never share a working tree; no work lost on close.

**B2 — the fabric (the differentiator).**
- MCP spawn/manage tools, Task-shaped + capability-gated (§4), with the optional
  `result_schema` contract (§4).
- Orchestration skill.
- Review/merge queue: `apply`/`merge` verbs, serialized integration, checks gates (§7).
- *Exit:* the orchestrator can spawn/collect a cross-harness fleet; the human reviews &
  merges from the queue.

**B3 — PTY composite (the shell).**
- `TerminalBackend` + wezterm-term; host the orchestrator pane; locked-mode input; OSC
  passthrough; capability scoping; resize recovery (§8a, §9). Gated on the §11 spike.
- *Exit:* the orchestrator's native TUI renders full-fidelity inside the bitrouter frame.

**B4 — attach + non-ACP fallback (cuttable).**
- `attach` for native-fidelity driving of an ACP subagent (may land *before* inference if
  fidelity demand appears). herdr-style status inference for opaque agents (§10) is the
  non-ACP path and is cuttable from v1.

**Polish (parallel, lower-risk).**
- Composer (atomic elements, Enter/Shift-Enter via kitty, `@`-mentions, draft-snapshot
  across pane switches), mouse, cross-thread "jump to requesting agent," HistoryCell
  compact/full.

**Deferred / non-goals:** container-per-agent isolation; a full task-DAG scheduler
(Anthropic's caution: coding is less parallelizable than research — favor isolated,
non-overlapping tasks + a disciplined gate over a fancy scheduler); config hot-reload.

---

## 14. Standing gates (carried forward, re-verified every iteration)

**Mechanical:** `cargo nextest run --all-features`; `cargo clippy --all-features` zero
warnings; `cargo fmt --check`; no `unwrap`/`expect`/`panic!`/`#[allow]`/dead code/public
re-exports in `src/tui/`.

**Polish rubric:** alignment (unicode-width-correct, `…` overflow); color (semantic,
glyph-not-color-alone, `NO_COLOR`, 16-color degrade); liveness (spinner, frame coalescing,
<~100ms keypress, changed-row cue); edge states (0/1-agent, pre-first-output, dead agent,
tiny terminals down to 20×5, degrade-never-crash); focus/input (unmistakable focus, cursor,
scrollback indicator, hint line matches live bindings); **terminal restore on panic**.

**New for v2 — fidelity gates (PTY panes):** the §11 target-terminal matrix passes;
Ctrl-C interrupts (not quits); OSC-52 copy round-trips; resize doesn't corrupt the inner
TUI or lose status detection. VT100/snapshot tests for the ACP-event render path.

---

## 15. Resolved decisions (review log)

Resolved after a Fable-5 review grounded in `origin/main` + the substrate. Don't reopen
without reason.

1. **CLI surface → keep `bitrouter tui`** for the composite manager; **do not fold into
   `launch`** and do not add a verb. `launch`'s transparent inherit-stdio contract must
   survive as the fallback when the §11 fidelity spike fails on a terminal; `tui` is
   unreleased, so re-scoping it (agent = orchestrator *harness id*, not a config `agents:`
   entry) breaks nothing. Lockstep: rewrite the `tui` rows in
   `skills/bitrouter/references/cli.md` (the "Ctrl-C exits any mode" / "no daemon" lines are
   now false) + `CLI.md`.
2. **MCP transport → stdio `bitrouter mcp serve` subprocess**, a `fleet` backend that
   clients the fleet daemon, injected by the manager. HTTP is a later additive mount;
   mutating fleet tools must not ride the unauthenticated HTTP→local path (§4).
3. **Daemon boundary → a distinct per-repo `bitrouter fleet serve`**; `bitrouter serve`
   stays the LLM proxy. Different scope (repo vs. global) and blast radius; `serve` restarts
   routinely and holds keys (§2).
4. **Fleet size → 1 orchestrator + ~2–6 ACP subagents; no opaque-native panes in v1.** The
   whole moat (real decision queue, risk tiering, review) exists only for ACP; the opaque
   path is a large zero-reuse surface. Pull `attach` ahead of inference if fidelity demand
   appears (§13-B4).
5. **Result schema → yes, goose-style `response.json_schema`, optional, landing with B2.**
   Collection *is* B2's feature; the change is an additive NDJSON field + repair loop (§4).

**Folded-in review corrections:** the daemon reversal is sized as real substrate work
(§2, §7), not glue; the §5/§7 `merge` contradiction is fixed (writes human-gated by
default); permission fan-out over the single-consumer stream is called out (§5); the
worktree default-flip has a migration story and the bootstrap hook is gated (§6); the
orchestrator-dies-with-the-TUI asymmetry is named (§2); the Windows detach gap is scoped
(§11).

**Still genuinely open (smaller):** exact fleet-tool names (reconcile with SPAWN_SPEC §11);
whether `AllowThread` policy is per-agent or per-fleet; the fleet daemon's idle-shutdown
default.
```
