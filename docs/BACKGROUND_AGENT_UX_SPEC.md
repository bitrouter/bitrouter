# Spec: inline multi-agent control deck for BitRouter Code

Status: **implemented; locally verified** · Revision 3 · Date: 2026-09-20

Implementation was authorized after review. Progress and verification evidence
are tracked in [BACKGROUND_AGENT_IMPLEMENTATION.md](BACKGROUND_AGENT_IMPLEMENTATION.md).
The acceptance criteria below remain requirements, not claims of completed work.

This document proposes a hybrid terminal UX for running and supervising
multiple ACP agent sessions without turning the ordinary `bro code`
conversation into a permanent full-screen application.

The central product decision is:

> Keep foreground session history in the terminal's native scrollback. Keep
> background-agent awareness and the command center at the bottom of the inline
> Code surface. Enter the alternate screen only when the requested content
> cannot be represented safely in that bounded bottom dock.

The default experience is therefore not “inline conversation plus a separate
Agent Hub.” It is one inline surface with two ownership regions:

```text
terminal-owned document plane       foreground session history
BitRouter-owned control deck        composer, foreground status, agent strip
```

The control deck is always present while `bro code` owns the terminal. It is
collapsed by default and expands in the normal buffer for ordinary multi-agent
management. Full background history, long retained detail, and complex review
enter an alternate-screen inspector only after an explicit user action.

The product direction and decisions in section 2 are approved. The subsequent
implementation request authorizes the delivery phases below; release and remote
agent execution remain outside that request.

---

## 1. Relationship to existing specs and current behavior

### 1.1 Implemented baseline retained by this proposal

[`CODE_TUI_UX_SPEC.md`](CODE_TUI_UX_SPEC.md) remains authoritative for the
ordinary foreground conversation:

- one conversation rather than permanent dashboard tabs;
- the normal terminal buffer and native scrollback;
- one variable-height dock for composition and contextual controls;
- `Ctrl-P` for the action inventory;
- `Ctrl-O` for the retained foreground transcript and detail inspector;
- `F2` for permissions, `F3` for queued follow-ups, and `F4` for the selected
  detail entry;
- agent, confirmed route, activity, and attributed cost as default foreground
  session facts; and
- terminal restoration, permission focus, queue recovery, cancellation, and
  draft preservation contracts.

This proposal does not add a permanent sidebar, fixed top bar, global activity
feed in scrollback, or selectable full-screen renderer for ordinary foreground
Code.

### 1.2 Deliberate amendments if approved

The current Code spec lists a multi-session workspace and background ACP
supervisor as non-goals. This proposal reopens only those exclusions.

It also refines the existing detached-surface rule:

- bounded global awareness and common background actions belong in the normal
  buffer's bottom dock;
- large retained background content may use an explicitly opened alternate
  screen; and
- the alternate screen is an inspector/attachment escalation, not the default
  global management destination.

[`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md)
already says a supervisor becomes justified when an accepted requirement needs
a controller to continue after its client exits, attach/detach, or durable
background work. This feature supplies that trigger. Its ownership rules still
apply:

- the controller/session kernel stays below CLI and presentation;
- the TUI remains a client rather than the process or persistence owner;
- the harness-native session ID remains authoritative;
- BitRouter does not invent a replacement canonical transcript; and
- remote ACP remains a separate transport and security decision.

### 1.3 Pre-implementation limitation

At the design baseline, the `bro code` process owned one `SessionHandle`,
cancelled its jobs on exit, and shut the handle down. A renderer-only change
cannot make a turn survive terminal closure. Background execution requires
process ownership to move into a resident supervisor before the UI promises
detach or later attachment.
Current implementation and test evidence are tracked separately in the
[implementation ledger](BACKGROUND_AGENT_IMPLEMENTATION.md).

---

## 2. Decisions for review

| ID | Proposed decision | Consequence |
| --- | --- | --- |
| D1 | Keep a newly opened foreground Code conversation inline and scrollback-native. | Multi-agent support does not force ordinary Code into full-screen mode. |
| D2 | Divide inline Code into a terminal-owned document plane and a BitRouter-owned bottom control deck. | Foreground history remains native; interactive and cross-session state has one bounded home. |
| D3 | Keep a one- or two-line background-agent strip visible at the bottom while Code owns the terminal. | Global awareness is ambient without permanently showing every agent row. |
| D4 | Let `F5` expand/collapse the background command center inside the normal buffer; expose the same action through `Ctrl-P`. | It avoids current `F2` permission, `F3` queue, and `F4` detail bindings and does not change screen mode for routine management. |
| D5 | While the command center is expanded, cap the **entire control deck** at 40% of the viewport, including borders, draft summary, status, rows, peek/editor, and help. | The foreground editor collapses to a non-editable draft summary while agent focus is active; at least 60% remains a document plane. |
| D6 | Never auto-expand the command center because of a background event. | Attention changes the collapsed strip and optional terminal notification, but does not steal focus or move the user's document unexpectedly. |
| D7 | Keep foreground and per-background reply drafts distinct and visibly target-bound. | Selecting another row can never silently redirect the foreground composer. |
| D8 | Use list row, inline peek/quick action, then alternate-screen attach as progressive disclosure. | Most background work can be managed without leaving the inline surface. |
| D9 | Enter alternate screen only for retained background history, history search, long retained output/diffs, complex permission review, or standalone management without a foreground document. | Full-screen ownership is an explicit detail escalation rather than the default global view; any retention gap is visible. |
| D10 | Model a background item as a BitRouter-supervised ACP conversation, not a PID. | It has explicit lifecycle, native identity, permissions, route/cost evidence, and attach semantics. |
| D11 | Make every target-state Code session, including the foreground session, daemon-supervised; give each clean exit an explicit stop-or-detach disposition. | Process ownership is uniform, foreground runs participate in directory claims, and detaching does not depend on transferring an in-process handle. |
| D12 | Allow only one generation-fenced interactive control lease per supervised run, independent of screen mode. | Inline quick actions can acquire a transient lease without attaching; stale clients cannot mutate after takeover. |
| D13 | Derive activity from protocol and supervisor events and repaint collapsed state only on meaningful transitions. | Background work does not create a high-frequency feed that disrupts native scrollback. |
| D14 | Treat `end_turn` as **Ready for review**, not proof that the user's task completed. | BitRouter does not claim completion ACP cannot establish. |
| D15 | Preserve permission focus safety across sessions: arrival never steals focus, no choice starts selected, and every response binds stable run/permission/lease identities. | Existing policy may answer; otherwise the exact request becomes Needs input without allowing buffered input to approve it. |
| D16 | Do not infer agent-native child agents from process trees, ANSI output, or logs. | Native child activity appears only through a negotiated typed capability; BitRouter-supervised runs always appear. |
| D17 | Make the first release local to one BitRouter daemon and configuration context. | Remote sessions, cross-host aggregation, and Cloud fleet management remain separate work. |
| D18 | Block a second potentially writable run in the same canonical Git worktree root, or canonical directory outside Git, by default. | Different subdirectories of one worktree still collide; separate worktrees remain independent unless the user makes an explicit warned override. |
| D19 | Persist only a minimal run ledger; retain the live transcript/event journal only for the supervisor lifetime. | After daemon restart, a formerly active row is Interrupted, not silently relaunched or presented as a durable BitRouter transcript. |
| D20 | Keep global budget/usage out of the collapsed strip unless scope and attribution are exact. | Ambiguous values such as “weekly 3B tokens” do not appear as precise but misleading status. |
| D21 | Give `bro run --background` an interactive-wait permission default distinct from ordinary headless `run`'s deny-all default. | Unmatched requests become Needs input when a supervisor can broker them; explicit headless permission flags still override deterministically. |

Changes to D1-D21 require explicit design review. Implementation progress and
verification evidence belong in a separate ledger or implementation plan.

---

## 3. Research synthesis and revised direction

### 3.1 What remains useful from Codex-style multi-agent UX

Codex keeps spawned-agent activity subordinate to the active conversation. A
picker or overview helps inspect children without turning every agent into a
permanent peer tab. This supports the selected “one foreground focus” model.

Relevant source snapshots:

- [Codex multi-agent picker source](https://github.com/openai/codex/blob/rust-v0.155.1/codex-rs/tui/src/multi_agents.rs)
- [Codex agent overview renderer](https://github.com/openai/codex/blob/rust-v0.155.1/codex-rs/tui/src/app/agents_overview_render.rs)
- [Codex subagents documentation](https://learn.chatgpt.com/docs/agent-configuration/subagents)

### 3.2 What remains useful from Claude Code

Claude Code demonstrates that global session status, lightweight peek, and
attach/detach are distinct depths of interaction. It also demonstrates why a
background transcript may need application-owned history: output produced
without an attached terminal cannot already exist in that terminal's native
scrollback.

Relevant official documentation:

- [Claude Code Agent View](https://code.claude.com/docs/en/agent-view)
- [Claude Code subagents](https://code.claude.com/docs/en/sub-agents)
- [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams)
- [Claude Code fullscreen mode](https://code.claude.com/docs/en/fullscreen)

### 3.3 Revision from the first draft

The first draft made a full-height Agent Hub the primary global management
surface. This revision rejects that default. Global state is not a peer
destination to the conversation; it is a control plane underneath it.

```text
first draft                              selected revision
──────────────────────────────────       ────────────────────────────────
inline foreground conversation           inline foreground document
attention-only footer                    always-visible agent strip
F5 -> full-height Agent Hub              F5 -> expanded bottom command center
peek inside Hub                          inline peek/quick actions
Enter -> attached full-height session    Enter -> attached full-height session
```

The alternate-screen attachment remains because native scrollback cannot
cleanly switch among several independent session histories. The global list
does not require that escalation.

---

## 4. Goals and non-goals

### 4.1 Goals

1. Keep the ordinary reading and writing loop focused on one foreground agent.
2. Make background-agent attention continuously discoverable at the bottom of
   Code without creating a permanent full list.
3. Resolve common background actions without leaving the normal buffer.
4. Let a supervised turn continue after its Code client or terminal exits.
5. Make every BitRouter-supervised run discoverable from a local client using
   the same daemon/configuration context.
6. Let users inspect exact pending questions, permissions, settled results,
   failures, and deterministic activity.
7. Attach to available retained background history, with an explicit gap marker
   when earlier events were evicted.
8. Preserve foreground draft, queue, permission, cancellation, route, cost,
   and terminal-restoration contracts.
9. Avoid background repaint churn that degrades native scrollback reading.
10. Make concurrent repository work safe by default.
11. Work on narrow terminals and without mouse input.
12. Provide a scriptable CLI over the same supervisor contract.

### 4.2 Non-goals

- Making ordinary `bro code` full-screen.
- A permanent left/right sidebar or top status bar.
- Permanently displaying every background agent in the collapsed state.
- Writing background activity or transcripts into foreground scrollback.
- Switching several complete session histories inline as if they were GUI tabs.
- A continuously updating token/activity ticker.
- A graphical task manager, web dashboard, or Cloud fleet view.
- Remote ACP execution or cross-host aggregation in the first release.
- Discovering arbitrary processes with `ps`, PTYs, process names, logs, or
  model-generated heuristics.
- Guaranteeing visibility into Codex/Claude internal child agents without an
  explicit typed adapter capability.
- A BitRouter-owned canonical transcript database.
- Automatic worktree creation, branch merging, or conflict resolution.
- Automatically approving permissions or synthesizing replies.
- Treating a settled ACP turn as verified task completion.
- tmux/iTerm split panes as the primary multi-agent UX.
- Multiple simultaneous writers to one ACP session.
- Keeping a fixed BitRouter footer after the user exits back to their shell.

---

## 5. Product vocabulary

| Term | Meaning |
| --- | --- |
| document plane | The upper normal-buffer region containing foreground session history. The terminal owns scrollback, selection, and ordinary reading position. |
| control deck | The complete BitRouter-owned bottom region: foreground composer, foreground status, collapsed agent strip, and optional expanded command center. |
| agent strip | One- or two-line always-visible summary of background attention/activity while Code owns the terminal. |
| command center | The expanded, internally scrollable background-session list and quick-action surface inside the normal-buffer dock. |
| foreground conversation | The one inline ACP conversation whose history is emitted into the current terminal's scrollback. |
| supervised run | One controller/adapter process and ACP conversation owned by the BitRouter supervisor. It may contain several turns. |
| background session | User-facing name for a supervised run with no foreground inline document attached. |
| agent run ID | BitRouter identifier for the supervised controller lifetime. It never replaces the harness-native session ID. |
| native session ID | Session identifier issued and owned by the ACP adapter/harness. |
| inline peek | Bounded command-center detail for the latest exact question, permission, result, failure, or activity. |
| background inspector | Explicit alternate-screen surface for retained history (with visible gaps), search, long retained detail, or complex review. |
| attach | Acquire/resume the interactive control lease and open a background session in the inspector. |
| detach | Return from the inspector while the supervisor keeps the run alive. |
| child task | Work spawned internally by an agent. It is not a separate run unless represented by a typed capability or BitRouter supervisor. |

Product copy normally says **agent** or **background session**, not “process.”
Process identity belongs in diagnostics.

---

## 6. Surface and ownership model

### 6.1 Document plane

The document plane is the existing scrollback-native foreground history. It
accepts only foreground conversation output and explicitly requested inline
artifacts. Background events never append rows there.

BitRouter cannot read or restore the user's native scrollback offset. The
control deck therefore avoids unnecessary repaint/output activity while the
user may be reading above the live tail.

### 6.2 Collapsed control deck

The collapsed deck contains:

1. foreground composer;
2. foreground status;
3. background agent strip; and
4. contextual help.

The agent strip shows, in priority order:

- the most urgent exact attention item, if any;
- the number of additional sessions needing input;
- newly Ready count;
- Working count; and
- the `F5 Agents` affordance.

It does not show all rows, per-second ages, spinners, tool-call streams, or
global usage totals.

### 6.3 Expanded command center

`F5` expands the command center inside the bottom dock. It:

- gives the entire control deck, not merely the agent rows, at most
  `floor(viewport_rows × 0.40)` physical rows;
- counts borders, foreground draft summary, compact status, agent rows,
  peek/reply editor, and help against that budget;
- replaces the editable foreground composer with a one-line, non-editable
  **Foreground draft preserved · N lines** summary while agent focus is active;
- leaves at least 60% of the viewport as the document plane;
- scrolls its own row list;
- groups attention before Ready, Working, Idle, and Stopped;
- offers inline peek and typed quick actions;
- maintains its own selection/filter/focus; and
- collapses back to the same foreground draft and cursor.

Allocation priority inside the expanded budget is: mandatory permission or
lease error, target-bound editor/peek, selected agent row, additional agent
rows, compact foreground/background counts, then help. Lower-priority rows
collapse before the document plane does. At the supported 40×16 minimum, the
expanded deck therefore uses at most six physical rows and shows a compact
draft summary rather than the multiline editor.

The writer cannot pull transcript rows back out of native scrollback when the
dock shrinks. Collapse guarantees no background text was appended or replayed,
and restores draft bytes, cursor, focus, selection, and filter. It does not
promise to restore the user's prior native-scrollback viewport or remove blank
rows left by the larger dock.

The command center never auto-expands. A new attention event changes the agent
strip and may emit one terminal notification/bell according to user settings,
but does not steal focus.

### 6.4 Alternate-screen escalation

The alternate screen is justified when at least one of these is true:

- the user attaches to retained background history;
- the user searches a background transcript;
- retained tool output or a diff exceeds the bounded inline peek;
- a permission requires long context or choices that do not fit safely;
- the user requests a full cross-session inspector or batch operation; or
- `bro agents` is launched standalone without a foreground document plane.

Routine status, short questions, bounded permission choices, cancellation,
mark-reviewed, and new-run dispatch stay in the normal-buffer command center.

### 6.5 Progressive disclosure

```text
always-visible agent strip
        │ F5
        ▼
expanded bottom command center
        │ Space / quick-action key       │ Enter attach/detail
        ▼                                ▼
inline peek/action                 alternate-screen inspector
exact bounded state                retained content + visible gaps
```

---

## 7. Wireframes

The wireframes define hierarchy and interaction, not final colors or exact
spacing. `F5` is the proposed shortcut, subject to terminal QA.

### 7.1 Default inline Code with collapsed agent strip

```text
You
  Please update the retry policy and run the tests.

Assistant
  I found the retry loop in src/client.rs and am checking its callers.

  ● Read src/client.rs
  ● Search retry_budget

┌ Next turn ──────────────────────────────────────────────────────────────┐
│ Also keep the public error wording unchanged.                           │
└─────────────────────────────────────────────────────────────────────────┘
FG codex · gpt-6-astra via auto · working · ctx 135k/200k · $0.06 router
BG ! docs-audit needs input · +1 attention · 1 ready · 4 working · F5 Agents
```

When there is no background work:

```text
BG no background agents · F5 Agents
```

When no session needs input but work continues:

```text
BG 1 ready · 4 working · F5 Agents
```

The strip is event-driven. It does not update a running timer or token counter
every second.

### 7.2 Current foreground permission remains highest priority

```text
┌ Permission ─────────────────────────────────────────────────────────────┐
│ Run `cargo nextest run --all-features`                                  │
└─────────────────────────────────────────────────────────────────────────┘
FG codex · permission needed · $0.06 router
F2 Permission (1) · BG 2 need input · F5 Agents · Ctrl-P Commands
```

The current session's permission notice remains visible, but arrival never
steals focus. `F2` is still the only ordinary transition into foreground
permission focus. While one is pending, opening the background command center
is allowed only for read-only status; reply, permission, attach, cancel, stop,
and new-run actions are disabled until the foreground permission is resolved
or explicitly denied. Background attention never replaces it.

### 7.3 Expanded bottom command center

This normal-width example assumes a viewport tall enough for a twelve-row
control-deck budget. The foreground editor is preserved but replaced visually
by a one-line summary while agent focus is active.

```text
Assistant
  I found the retry loop in src/client.rs and am checking its callers.

  ● Read src/client.rs
  ● Search retry_budget

┌ Foreground draft preserved · 1 line ───────────────────────────────────┐
├ Agents · 2 need input · 1 ready · 4 working ───────────────────────────┤
│ › ! docs-audit   claude   Question: update Chinese page?           8m  │
│   ! auth-review  codex    Permission: cargo nextest                3m  │
│   ● retry-fix    codex    Turn ready for review                    1m  │
│   ◌ route-tests  codex    Running cargo nextest                    2m  │
│   ◌ 3 more working                                                     │
├─────────────────────────────────────────────────────────────────────────┤
│ ↑↓ Select · Space Peek · R Reply · Enter Attach · N New · F5 Collapse  │
└─────────────────────────────────────────────────────────────────────────┘
```

The foreground draft remains unchanged but its editable body is hidden while
focus is inside the command center. Collapsing restores the editor bytes and
cursor; it does not promise to reconstruct the terminal's previous viewport.

### 7.4 Inline peek: short question

```text
├ docs-audit · Needs input · claude · ~/bitrouter-docs ──────────────────┤
│ The English page changed. Should I update the Chinese page in this run? │
│                                                                         │
│ R Reply · Enter Attach · Space Close                                    │
└─────────────────────────────────────────────────────────────────────────┘
```

Pressing `R` opens a separate target-bound reply editor:

```text
├ Reply to docs-audit · background ──────────────────────────────────────┤
│ Yes. Keep sourceHash in lockstep._                                      │
├─────────────────────────────────────────────────────────────────────────┤
│ Enter Send to docs-audit · Esc Keep draft and return                    │
└─────────────────────────────────────────────────────────────────────────┘
```

The label contains both the run name and **background**. The foreground
`Next turn` draft is a different buffer.

### 7.5 Inline peek: bounded permission

```text
├ auth-review · Permission · codex · ~/bitrouter-a32f ───────────────────┤
│ Run `cargo nextest run --all-features`                                  │
│                                                                         │
│   Allow once                                                            │
│   Deny                                                                  │
├─────────────────────────────────────────────────────────────────────────┤
│ F2 Focus choices · A Attach details · Space Close                       │
└─────────────────────────────────────────────────────────────────────────┘
```

Peek is initially read-only: no option is selected and Enter has no effect.
`F2` explicitly acquires permission focus, consumes that keypress, clears any
old highlight, and only then enables arrow selection. The choices are the
exact controller/policy choices; inline peek does not invent an “always allow”
option. If the command, policy context, or choices do not fit safely, `F2`
opens the alternate-screen permission inspector instead.

### 7.6 Alternate-screen attached background session

```text
 Agent Inspector / auth-review       codex · background · needs input
──────────────────────────────────────────────────────────────────────────
You
  Audit the auth changes and run the relevant tests.

Assistant
  I found one behavior change in token refresh. The focused tests pass.

  ● Read src/auth.rs
  ● Run cargo nextest run -p bitrouter auth

Permission requested
  Run cargo nextest run --all-features

──────────────────────────────────────────────────────────────────────────
> _
F2 Permission · Ctrl-O Details · Esc Interrupt · F5 Detach to Code
```

This surface owns history/search because its transcript was produced without
an attached terminal. `F5` detaches and returns to the inline Code surface;
it does not cancel or stop the run.

### 7.7 Narrow terminal

Collapsed:

```text
FG codex · working
BG !2 · ●1 · ◌4 · F5
```

Expanded:

```text
Agents · 2 need input
› ! docs-audit
    claude · 8m
  ! auth-review
    codex · 3m
  ● retry-fix · 1m

Space Peek · Enter Open · F5 Back
```

Below the supported minimum viewport, Code shows a deterministic resize
message. It never clips permission choices or enables a hidden destructive
action.

---

## 8. Agent grouping and row content

### 8.1 Groups

The expanded command center groups rows in this priority:

1. **Needs input** — permission, explicit question, or failure requiring a
   decision;
2. **Ready for review** — a turn settled and the result is unread;
3. **Working** — submitting, running, or cancelling a turn;
4. **Idle** — live session with no active turn or unread result; and
5. **Stopped** — explicitly stopped or interrupted historical metadata.

Pinning affects order inside a group. It never moves a low-priority item above
Needs input. Stopped rows are collapsed by default.

### 8.2 Row content

A normal-width row shows:

```text
attention  label  agent  directory/worktree  deterministic activity  coarse age
```

Selected-row detail may show confirmed route, attributed cost, native session
ID, agent run ID, parent run, attached client, and failure reason. These do not
become permanent columns.

Activity text comes from explicit state:

- exact permission/question title;
- current tool title when available;
- `Working` when no more precise event exists;
- truncated final assistant result for Ready for review;
- exact sanitized failure; or
- `Idle`, `Stopped`, or `Interrupted`.

No inference request is made to summarize a row.

### 8.3 Completion language

ACP `end_turn`, a final assistant message, or an exited tool proves only that a
turn settled. The UI uses **Ready for review**. **Completed** may be introduced
only if a standardized agent signal or explicit user action establishes it.

---

## 9. Interaction contract

### 9.1 Focus and keyboard behavior

- Default focus stays in the foreground composer.
- `F5` moves focus into the expanded command center; if already expanded, it
  collapses and restores the prior foreground cursor/focus.
- `Ctrl-P → Background agents` performs the same expansion and is the
  authoritative discoverable action.
- Arrow keys move command-center selection only while it owns focus.
- `Space` opens/closes inline peek.
- `R` opens a target-bound background reply editor when reply is valid.
- `Enter` attaches or opens required long detail according to row state.
- `N` begins a new supervised background run with explicit target summary.
- `Esc` closes the innermost temporary editor/peek without discarding another
  draft.

Function keys are conveniences, not the only pathway. Terminals that intercept
`F5` retain palette access.

### 9.2 Cross-session permission focus

Permission focus is surface-local but foreground-safe:

| Current surface | `F2` behavior |
| --- | --- |
| Inline conversation, command center collapsed | Focus the oldest foreground permission; background permissions remain summarized in the agent strip. |
| Expanded command center with a foreground permission pending | Return to/focus the foreground permission. Background mutations and attach remain disabled. |
| Expanded command center with no foreground permission | If the selected inline peek is a background permission, acquire a transient lease and focus that exact request; otherwise do nothing. |
| Background inspector | Focus that attached run's oldest permission. A later foreground permission produces a non-stealing banner; `F5` detaches, after which `F2` focuses foreground. |

A foreground permission pending before attach blocks entry to a background
inspector. If one arrives after the inspector opens, it never steals input or
changes the meaning of a buffered key. The inspector shows
**Foreground permission waiting · F5 return** until the user detaches.

Every permission response carries:

```text
agent_run_id
permission_id
option_id
lease_generation
action_request_id
```

The supervisor atomically verifies all five values before resolving the ACP
request. Enter has no effect until the user explicitly entered permission
focus and selected an option after that transition. Changing row, surface,
permission ID, or lease generation clears the highlight and invalidates any
in-flight local confirmation. Repeated `action_request_id` values are
idempotent; stale generations are rejected rather than replayed.

### 9.3 Draft ownership

The client maintains separate drafts:

```text
foreground_draft
background_reply_draft[agent_run_id]
new_background_run_draft
```

These names describe required semantics, not exact implementation types.

Requirements:

- expanding/collapsing never edits the foreground draft;
- changing selected background row never changes the foreground send target;
- every background editor names its target above the input;
- unsent drafts stay client-owned and are not silently persisted by the
  supervisor;
- exiting with a non-empty draft follows an explicit warn/keep/discard flow;
  and
- permission choices are typed actions, never free-text drafts.

### 9.4 Starting background work

`N` opens a bounded target editor showing:

- resolved agent;
- canonical working directory/worktree;
- requested route;
- directory-claim conflict, if any; and
- initial prompt.

Submission succeeds only after the supervisor accepts ownership and returns an
agent run ID. If no safe agent/directory default exists, the prompt is disabled
until the user selects one.

### 9.5 Inline quick actions

Inline peek may mutate a session only through actions valid for its current
typed state:

- choose one exact permission response;
- answer an explicit agent question;
- send a follow-up after a settled turn;
- mark a result reviewed;
- cancel the active turn; or
- request a confirmed stop.

If another client owns the control lease, peek is read-only and identifies the
owner without stealing control. Lease takeover is a confirmed recovery action
and uses the inspector when context cannot fit safely.

The lease is independent of screen mode:

1. read-only list and peek require no lease;
2. entering a reply editor, permission focus, cancel confirmation, or stop
   confirmation atomically acquires a transient lease when it is unowned or
   already owned by this client;
3. every mutation carries the acquired `lease_generation` plus a stable
   action/request ID;
4. a one-shot permission, cancel, mark-reviewed, or stop releases a lease that
   was acquired transiently after the supervisor acknowledges the action;
5. a reply editor holds the lease through accepted submission, then releases
   it unless the user attaches; and
6. attach retains/upgrades the lease until detach.

A confirmed takeover increments the generation before granting control. Any
old client's in-flight reply, approval, cancellation, or stop is then rejected
even if it arrives later.

### 9.6 Attach and detach

Attaching a background run:

- acquires the control lease;
- enters the alternate-screen inspector;
- receives an atomic replay snapshot and monotonic live events;
- keeps the original foreground document and draft intact underneath; and
- does not print the background transcript into native scrollback.

Detaching:

- leaves controller, adapter, active turn, route lease, and metering alive;
- releases/transfers the control lease according to supervisor policy;
- restores normal-buffer modes;
- returns to the same command-center row and filter; and
- preserves the foreground draft/cursor.

Stopping is a separate confirmed action. It cancels active work, shuts down the
controller/child deterministically, and records lifecycle state. It never
deletes a harness-native saved session.

Removing is allowed for stopped, failed, or interrupted rows after child reaping
has settled. `stop` against an already failed/exited child is an idempotent
cleanup operation: it reaps any residual resources but does not rewrite the
failure as success. Remove deletes BitRouter run metadata and ephemeral
retained events, not the native harness session.

### 9.7 Leaving Code

The control deck exists only while `bro code` owns the terminal. Exiting back
to the shell removes the deck but does not stop already-background runs.

All target-state Code sessions are supervisor-owned, including the foreground
session. Clean exit retains current intent by making disposition explicit:

- `Ctrl-C` or `Ctrl-D` from Ready with an empty draft stops the foreground
  controller and exits, matching today's clean-exit outcome;
- **Detach current session and exit** leaves it running and converts it to a
  background row;
- while a turn is active, `Ctrl-C`/Escape requests cancellation rather than
  exit; the explicit detach action is the path that leaves it running; and
- terminal/client loss is treated as detach after lease expiry, never as an
  implicit stop.

Persisting a fixed footer in the user's shell would require separate shell or
terminal-multiplexer integration and is not part of this proposal. Users can
reopen Code, run standalone `bro agents`, or attach directly by agent run ID.

---

## 10. Repaint and native-scrollback safety

Normal-buffer applications cannot reliably know whether the user has scrolled
away from the live tail. Some terminals jump on output; others preserve the
scroll position. Background activity must therefore avoid unnecessary terminal
writes.

### 10.1 Collapsed strip update policy

The collapsed strip repaints only when displayed information changes because
of a meaningful transition:

- a run enters/leaves Needs input;
- a run first becomes Ready for review;
- a run starts/stops/fails/is interrupted;
- aggregate counts change; or
- the user expands/collapses or changes scope.

It does not repaint for:

- every streamed token;
- every tool progress chunk;
- spinner frames;
- elapsed seconds;
- continuously changing token totals; or
- activity events that do not change the collapsed summary.

### 10.2 Expanded command-center policy

While focused, the command center may show current tool labels and coarse age,
but it coalesces high-frequency events. Exact cadence is an implementation
measurement, not a product timer contract. No event may append background text
to the document plane.

### 10.3 Usage and cost display

Foreground status may show only labelled facts with clear scope/source, for
example:

```text
ctx 135k/200k
session $0.06 router-attributed
```

The collapsed background strip prioritizes actionability over usage. A global
budget may appear later only when principal, time range, included agents,
source, and reset semantics are exact. A bare `weekly 3B tokens` value is not
acceptable.

---

## 11. State and attention semantics

One overloaded `status` string is insufficient. The supervisor exposes
orthogonal dimensions:

```text
process      starting | running | stopping | stopped | failed | interrupted
turn         idle | submitting | working | cancelling
attention    none | question | permission | result | error
attachment   detached | observed | controlled(client-id)
review       unread | reviewed
```

The display group is derived:

| Condition | Group |
| --- | --- |
| permission/question/error requiring action | Needs input |
| settled result and unread | Ready for review |
| submitting/working/cancelling | Working |
| running + idle + no unread attention | Idle |
| stopped/interrupted metadata | Stopped |

Priority is `Needs input > Ready > Working > Idle > Stopped`. Monotonic event
handling prevents an older event from moving a row backward incorrectly.

The collapsed strip includes other runs in Needs input, newly Ready runs, and a
stable Working count. It never counts the foreground session as background.

---

## 12. Supervisor and ownership architecture

### 12.1 Required dependency direction

```text
inline Code + control deck ─┐
background inspector ───────┼── local versioned session stream ── Supervisor
bro run --background ───────┘                                      │
                                                                  ▼
                                                      SessionHost/controller
                                                                  │
                                                                  ▼
                                                        ACP adapter/harness
```

The supervisor belongs in the BitRouter application/daemon layer. It owns:

- controller and adapter process lifetime;
- agent run IDs and native session ID mapping;
- the single control lease;
- live event sequencing and replay snapshot;
- pending permission/question state;
- route and attributed-cost binding;
- deterministic shutdown/reaping; and
- the minimal run ledger.

The `bitrouter-tui` crate owns reducer state, rendering, input mapping, drafts,
and typed client effects. It must not read configuration, the database, process
tables, or supervisor files directly.

### 12.2 Session ownership and exit disposition

Every ACP controller launched by target-state `bro code` or
`bro run --background` is supervisor-owned from creation. Foreground and
background describe presentation/attachment, not different process owners.

| Presentation state | Process owner | Interactive lease | Clean client exit | Unexpected client loss |
| --- | --- | --- | --- | --- |
| Foreground inline Code | Supervisor | Code client | Ready + empty-draft `Ctrl-C`/`Ctrl-D` stops; explicit detach keeps running | Lease expires; run continues detached |
| Background row/peek | Supervisor | None unless a transient action acquires it | Client exit has no lifecycle effect | No lifecycle effect |
| Attached background inspector | Supervisor | Inspector client | `F5` detaches; explicit stop terminates | Lease expires; run continues detached |

Because the supervisor owns the foreground controller too, directory claims,
route/cost attribution, permissions, event sequencing, and cleanup apply before
the user creates the first background run. There is no in-process
`SessionHandle` transfer when the foreground becomes background.

### 12.3 First-release lifetime promise

A supervised run survives:

- an explicit detach from inline Code;
- leaving the background inspector;
- unexpected loss of one terminal/client; and
- attaching a later local client to the same daemon context.

It does not promise to survive:

- explicit daemon shutdown;
- machine shutdown;
- supervisor crash; or
- migration to another host.

The durable ledger records enough metadata to mark a formerly active run
**Interrupted** after daemon restart. It does not contain a canonical
transcript and must not silently relaunch the agent. Loading a native saved
session is an explicit new-run action, not recovery of the old controller.

Explicit `--load`/`--resume` at supervised-run creation remains capability-led
and is distinct from automatic recovery after daemon restart. The first release
may use an advertised native saved session as its initial session selection;
it does not adopt or relaunch an interrupted controller automatically.

### 12.4 Event replay

For each live run, the supervisor retains an ephemeral ordered journal
sufficient to render the available retained portion in an attaching inspector.
Large tool payloads follow Code's bounded retention/detail rules.

The attach contract exposes:

```text
first_retained_seq
snapshot_seq
history_complete
```

`RunSnapshot` contains current lifecycle, turn, attention, lease, route/cost,
native identity, and unresolved action state at `snapshot_seq`; it is not a
second rendered transcript. `ReplayBatch` contains retained display events
from `first_retained_seq` through `snapshot_seq`, inclusive. A fresh attach:

1. replaces the client's prior run reducer/journal with the atomic snapshot;
2. applies each replay display event exactly once in sequence order; then
3. accepts only live deltas with sequence greater than `snapshot_seq`.

A sequence gap forces full replacement/resynchronization, never merge. If
`history_complete` is false or `first_retained_seq` is later than run start,
the inspector permanently renders **Earlier activity is not retained by
BitRouter** with a native-session history action when the adapter supports one.

Unresolved permissions/questions, their stable IDs/options, and an unread
terminal result cannot be evicted. Large tool bodies may be replaced by a
stable placeholder that identifies why the payload is unavailable. After a
run stops or fails, retained events remain until explicit remove or daemon
restart; after restart only the minimal ledger remains.

The journal may use bounded memory or daemon-runtime spill files, but it is
deleted with the supervised run and is not presented as the harness's durable
session history.

### 12.5 Single control lease

Exactly one client may submit prompts, answer permissions/questions, cancel,
or stop a run at a time. Other clients may receive read-only snapshots.

The lease has:

- owner identity;
- monotonically increasing generation;
- heartbeat/expiry policy;
- transient-action versus attached ownership mode;
- explicit release after action acknowledgement or detach;
- deterministic recovery after client loss; and
- an audited confirmed takeover path.

Client disconnect never implies session cancellation.

### 12.6 Worktree safety

The supervisor canonicalizes the requested directory before launch. Inside a
Git checkout, the claim key is the canonical worktree root, not the requested
subdirectory; outside Git it is the canonical requested directory. Every live
foreground or background run is treated as potentially writable in the first
release. Another run against the same claim key is blocked by default with
guidance to:

- select an existing separate worktree;
- create a worktree outside this feature; or
- make an explicit shared-directory override after a warning.

The first release does not create, delete, merge, or clean worktrees. Symlinks
and equivalent paths cannot bypass the collision check.

---

## 13. Visibility of spawned and native child agents

### 13.1 BitRouter-supervised runs

Any run created through the control deck, `bro run --background`, or a future
typed delegation action appears immediately. If a parent run creates it, the
ledger records `parent_run_id` for tracing and cost attribution. The first UI
may show this relationship in selected-row detail without a permanent tree.

### 13.2 Harness-native child agents

Codex, Claude, or another harness may create internal subagents inside one ACP
session. BitRouter cannot assume they are independent attachable sessions.

The first release shows only the parent run. If an adapter later negotiates a
typed child-activity capability, its row may show bounded detail such as
`3 child tasks · 1 needs input`. Separate rows require stable child identity,
lifecycle, attention, and control semantics. ANSI output, process ancestry,
log wording, and generated interpretation are not substitutes.

Therefore inline mode can show:

- all background runs spawned through BitRouter's supervisor; and
- only capability-backed summaries for opaque agent-native children.

---

## 14. Proposed CLI surface

Existing catalog/admin meanings remain valid:

```text
bro agents list
bro agents inspect <agent>
bro agents check [agent]
bro agents conformance <agent>
bro agents scaffold <agent>
```

The proposal adds supervised-run management:

```text
bro agents                         # standalone manager on a TTY
bro agents sessions [--json]       # list supervised runs
bro agents attach <agent-run-id>   # open background inspector
bro agents stop <agent-run-id>     # stop; do not remove native session
bro agents remove <agent-run-id>   # remove stopped/failed/interrupted metadata

bro run <agent> <prompt> --background
                                   # submit and return agent run ID
```

Bare `bro agents` is the standalone escape hatch when no foreground document
exists. It may use a full-height alternate-screen manager because the user
explicitly requested global management and there is no inline Code history to
preserve. It is not the default route from inside Code.

Non-TTY bare `bro agents` fails with guidance to
`bro agents sessions --json`; it emits no terminal control sequences into a
pipe.

`--background` is truthful only after the supervisor accepted ownership and
returned an attachable ID. It does not reuse the historical hidden
`--no-wait` behavior that immediately tore down its process-owned controller.

### 14.1 Background permission default

Ordinary headless `bro run` remains deny-all by default because no interactive
broker exists. `bro run --background` is different: the resident supervisor is
the broker, so an unmatched permission defaults to **Ask** and moves the run to
Needs input until an authorized client responds.

Explicit existing permission options override this background default:

| Option | Background behavior |
| --- | --- |
| `--approve-all` | Approve all requests without entering Needs input. |
| `--approve-reads` | Approve ACP read/search kinds and deny unmatched requests, preserving current explicit-mode semantics. |
| `--deny-all` | Deny all requests without entering Needs input. |
| `--permission-policy` with `defaultAction` | Use its explicit approve/deny default. |
| `--permission-policy` without `defaultAction` and no mode flag | Apply exact auto-approve/auto-deny matches; unmatched requests use background Ask. |
| no permission option | Background Ask. |

This requires a supervisor-aware permission mode; implementation must not
reinterpret `HeadlessOptions::default()` globally and accidentally change
ordinary `bro run`.

Other existing run options retain explicit semantics:

- `--turn-timeout` measures the supervised turn's wall-clock lifetime,
  including time waiting for permission; expiry cancels the turn and records a
  timeout failure rather than silently denying the permission;
- `--result-schema` validates the settled result before the row becomes Ready;
  validation failure becomes an exact error requiring review;
- explicit `--load`/`--resume` is allowed only when the selected adapter
  advertises the required native capability and is not automatic restart
  recovery; and
- hidden `--no-wait` conflicts with `--background` and is never its alias.

Exact flag spelling remains a public CLI review item. If implemented, the
`/bitrouter` Skill and agent-plugin manifests must change atomically with the
CLI, as required by the repository contract.

---

## 15. Permission, routing, cost, and security invariants

1. Backgrounding never broadens permissions. The same policy engine and exact
   controller permission choices apply.
2. A pending permission is supervisor state for the life of the run; losing
   the UI never auto-allows it.
3. Permission arrival never steals focus, and no choice starts selected.
   Responses are accepted only for the exact run, permission, option, lease
   generation, and idempotent action request.
4. Route identity is the confirmed session route, not merely the requested
   preset or current global default.
5. Cost remains attributed to the supervised run and, when present, its parent.
   The UI states whether cost is router-attributed or harness-reported.
6. Local attach/mutation uses an authenticated local session channel. Reusing
   an existing control endpoint does not authorize execution on the current
   read-only remote surface.
7. Logs/diagnostics redact prompts, secrets, tokens, and full tool payloads by
   default.
8. Stop and lease takeover are audited mutations.
9. Permission to list run metadata does not automatically grant transcript
   access; local authorization distinguishes list, peek, attach, respond,
   stop, and remove.

---

## 16. Failure, recovery, accessibility, and terminal behavior

### 16.1 Failure and recovery

| Failure | Required behavior |
| --- | --- |
| Clean `Ctrl-C`/`Ctrl-D` from Ready | Stop the foreground controller and exit; already-background runs continue. |
| Explicit detach or unexpected Code client loss | Foreground run becomes detached/background; lease expires or releases; work continues. |
| Terminal resize below minimum | Preserve all state; show resize view; do not hide permission choices. |
| Agent exits unexpectedly | Mark Failed, retain exact sanitized reason, reap child, surface attention. |
| Daemon restarts | Mark previously live ledger rows Interrupted; do not claim they run. |
| Event sequence gap | Stop applying deltas and request a fresh snapshot. |
| Lease lost mid-input | Preserve target-bound local draft, disable submit, identify current owner. |
| Permission arrives while detached | Mark Needs input and update agent strip; never auto-allow. |
| Route/cost evidence unavailable | Show `unknown` with source/reason; never copy another run's value. |
| Same-worktree claim collision | Block before launching an agent by default, including different subdirectories of one worktree. |
| Stop times out | Show Stopping/cleanup failure; never report Stopped prematurely. |
| Failed child is removed | Require settled reaping, then allow removal without rewriting the failure as success. |

### 16.2 Accessibility and terminal behavior

- Every action is keyboard-reachable; mouse support is optional.
- Color is not the only carrier of state.
- CJK, emoji, combining marks, and wide paths use measured terminal cell width.
- Focus order is foreground composer, agent list, peek/action, target-bound
  editor, then help.
- Screen-reader/live-region output announces meaningful transitions, not
  working timers or token increments.
- Alternate-screen, raw mode, cursor visibility, paste mode, and mouse mode are
  restored on normal exit, error, signal, and panic boundary.
- The dock has deterministic normal/narrow/minimum layouts; a permission cannot
  become actionable while one of its choices is clipped.
- Copy and transcript export are explicit inspector actions.

---

## 17. Delivery phases

### Phase 0 — approve contracts

- approve D1-D21 and vocabulary;
- freeze supervisor lifecycle, lease, event, ledger, and draft-target rules;
- decide exact CLI spelling and local authorization scopes; and
- produce an implementation plan with repository owners and migration order.

Completion criterion: renderer work does not begin against undefined process
ownership, attach, or input-target semantics.

### Phase 1 — headless supervisor foundation

- add supervised `SessionHost` outside the TUI crate;
- add run ledger, control lease, snapshot/delta stream, and deterministic
  shutdown;
- implement `bro run --background` and structured session listing; and
- prove explicit detach/client loss does not terminate an accepted run;
- freeze transient versus attached lease acquisition/release and generation
  fencing; and
- test takeover against delayed reply, permission, cancel, and stop requests.

Completion criterion: background work is real and scriptable before Code
advertises it.

### Phase 2 — collapsed agent strip

- connect Code to supervisor snapshots;
- add event-derived attention/Ready/Working aggregates;
- render the one-/two-line strip without background scrollback output;
- add `F5` and `Ctrl-P` discoverability; and
- measure scrollback behavior across supported terminals under background
  event load.

Completion criterion: ambient awareness does not degrade foreground editing,
streaming, scrolling, or terminal restoration.

### Phase 3 — expanded bottom command center

- add grouped rows, internal scroll, filter, inline peek, target-bound replies,
  bounded permissions, new-run dispatch, cancel, mark-reviewed, and stop;
- preserve foreground and per-background drafts independently;
- enforce the 40% **total control-deck** height cap, compact draft summary, and
  narrow layout;
- include the minimal alternate-screen permission/detail inspector required
  when a permission cannot fit the bounded dock; and
- test selection stability while rows change groups.

Completion criterion: common management stays in the normal buffer without
misrouting input or hiding foreground state.

Phase 3 is not releasable without that minimal escalation surface; it must not
render a clipped long permission as actionable.

### Phase 4 — full alternate-screen background inspector

- add retained history with gap markers, replay/resync, search, general long
  detail, attach/detach, copy/export, and standalone `bro agents`;
- preserve underlying Code state across every entry/exit path; and
- verify multi-client control lease and failure recovery.

Completion criterion: full background interaction is available without mixing
session histories in native scrollback.

### Phase 5 — optional typed child activity

- define an adapter capability for child identity/activity;
- expose bounded parent-row detail only when negotiated; and
- decide separately whether stable children deserve rows or a tree.

Completion criterion: no harness-specific parser or process heuristic exists.

Remote ACP, cross-host aggregation, automatic worktrees, split panes, and a
graphical workspace require separate specs.

---

## 18. Acceptance criteria

| ID | Criterion |
| --- | --- |
| A1 | Starting ordinary `bro code` remains on the normal buffer and preserves native scrollback. |
| A2 | Foreground history contains no output from background runs. |
| A3 | The collapsed agent strip remains at the bottom while Code owns the terminal and reduces deterministically on narrow terminals. |
| A4 | `F2`, `F3`, and `F4` retain existing permission, queue, and detail meanings; `F5` only expands/collapses agents or detaches from the inspector. |
| A5 | While expanded, the entire control deck—including border, draft summary, status, rows, peek/editor, and help—uses at most 40% of physical viewport rows. |
| A6 | Background events never auto-expand the command center or steal focus. |
| A7 | Collapsed state repaints only on meaningful displayed transitions; no spinner, per-second timer, token stream, or tool chunk drives it. |
| A8 | Foreground and background drafts are separate and every background editor visibly names its target. |
| A9 | Changing selected agent cannot change the destination of the foreground composer. |
| A10 | At 40×16 with a multiline foreground draft, queued follow-ups, and a selected background row, expansion keeps the draft intact, uses a compact summary, and leaves at least 60% document rows. |
| A11 | Permission arrival never steals focus; no choice starts selected; buffered Enter from another surface cannot approve a request. |
| A12 | Foreground and background permissions pending simultaneously follow §9.2, and every response validates run, permission, option, lease generation, and action request IDs. |
| A13 | Short question replies and bounded permission choices work inline; long content escalates without clipping or invented choices. |
| A14 | Retained background history/search opens only after explicit attach/detail action, never prints into native scrollback, and displays a permanent gap marker when earlier events were evicted. |
| A15 | Leaving the inspector restores terminal modes, draft/cursor, selection, and filter without promising restoration of native scrollback viewport or removal of blank dock rows. |
| A16 | All target-state Code sessions are supervisor-owned; clean `Ctrl-C`/`Ctrl-D`, explicit detach, and unexpected client loss follow the distinct §12.2 dispositions. |
| A17 | An accepted detached/background turn continues after every UI client closes. |
| A18 | Every background row comes from a supervisor record or negotiated typed child capability; no process scan creates rows. |
| A19 | A settled ACP turn is Ready for review, not Completed. |
| A20 | Exactly one generation-fenced control lease exists; transient inline actions can acquire/release it without attach. |
| A21 | After confirmed takeover, delayed mutations carrying the old lease generation are rejected. |
| A22 | Background permission choices exactly match controller/policy choices and are never auto-approved by the UI. |
| A23 | Detach keeps the run alive; stop is separate and confirmed; remove never deletes a native harness session. |
| A24 | Failed/exited runs can complete idempotent cleanup and then be removed without rewriting their failure state. |
| A25 | Agent, native session ID, confirmed route, and attributed cost remain unambiguous across attach/detach. |
| A26 | Ambiguous global token/budget values do not appear in the collapsed strip. |
| A27 | `bro run --background` succeeds only after supervisor ownership and an attachable agent run ID exist. |
| A28 | Background run permissions default to Ask only when no explicit permission mode/policy default exists; ordinary headless `run` remains deny-all. |
| A29 | Background `turn-timeout`, result-schema, load/resume, and hidden no-wait combinations follow §14.1. |
| A30 | Existing `bro agents list/inspect/check/conformance/scaffold` behavior remains compatible. |
| A31 | Non-TTY bare `bro agents` emits no terminal control sequences. |
| A32 | Foreground and background runs claim the canonical Git worktree root; two different subdirectories of one worktree collide before process launch. |
| A33 | Snapshot/replay/resync obey `first_retained_seq` and `snapshot_seq`, apply display events exactly once, and never evict unresolved actions or unread terminal results. |
| A34 | On daemon restart, formerly live rows are Interrupted rather than silently resumed or shown running. |
| A35 | Agent-native child processes remain inside the parent row unless a typed negotiated capability supplies stable semantics. |
| A36 | Minimum/narrow layout, CJK/emoji width, resize, suspend/resume, multiline paste, signals, and terminal restoration suites pass. |
| A37 | Remote contexts do not accidentally start local agents or expose execution through the read-only remote control surface. |
| A38 | The TUI crate contains no config, database, filesystem-ledger, process-scan, or control-server ownership. |
| A39 | Skills, manifests, CLI help, and public flags update atomically with implementation. |

---

## 19. Rejected alternatives

### 19.1 Full-height Agent Hub as the default global surface

Rejected because most background interactions are small and should not cause a
screen-mode transition. The bottom command center provides awareness, list,
peek, and quick action without abandoning the foreground document.

### 19.2 Permanent complete agent list

Rejected because agent count is unbounded, persistent rows consume reading
space, and high-frequency activity would create terminal churn. The permanent
surface is a one-/two-line summary; the complete list expands on demand.

### 19.3 Permanent left or right sidebar

Rejected because it consumes scarce width, harms code/diff readability, and
requires application ownership of the full viewport.

### 19.4 Global top status bar in inline mode

Rejected because a normal-buffer application cannot safely own a fixed row
above terminal-native scrollback. The top is not a stable application region.

### 19.5 Put the complete command center in shell scrollback

Rejected because dynamic rows would pollute history, duplicate state, and make
updates indistinguishable from foreground conversation output.

### 19.6 Reuse the foreground composer for selected background agents

Rejected because selection changes would silently change the prompt recipient.
Background replies use separately labelled, target-bound drafts.

### 19.7 Switch complete foreground histories inline

Rejected because native scrollback is one append-only terminal history. Replaying
another session would mix/duplicate transcripts and returning could not restore
a clean per-session document.

### 19.8 Make every Code session full-screen

Rejected because multi-agent awareness does not invalidate native scrollback
for the normal foreground conversation. Full-screen is reserved for content
that cannot be represented faithfully in the bounded dock.

### 19.9 Scan child processes

Rejected because a PID does not provide ACP identity, pending input,
permission semantics, route/cost attribution, attachability, or completion.

### 19.10 Treat native subagents as independent BitRouter runs

Rejected until adapters expose stable typed child identity and control. A
native child may be an internal task, thread, or process without an independent
attach contract.

### 19.11 Reuse `run --no-wait`

Rejected because returning before a process-owned controller shuts down is not
background execution. The truthful contract requires supervisor acceptance
and an attachable ID.

### 19.12 Let the TUI own the supervisor or ledger

Rejected because work must survive the TUI and be reachable from other
clients. This would also violate crate boundaries and create competing truth.

### 19.13 Generate friendly activity summaries with another model

Rejected for the first release because it adds latency, cost, privacy surface,
and non-determinism. Exact event-derived activity is sufficient.

### 19.14 Show global weekly tokens by default

Rejected unless principal, time window, included traffic, source, and reset
semantics are exact. A large unscoped number is visually impressive but not
operationally trustworthy.

### 19.15 Use tmux/iTerm panes as the primary design

Rejected because it depends on a terminal-specific orchestrator, scales poorly
beyond a few agents, and shows full transcripts when the immediate question is
usually which run needs attention.

---

## 20. Review checklist

Approval should explicitly confirm:

1. foreground history remains native scrollback and contains only the
   foreground conversation;
2. the agent strip is always present while Code runs but remains one or two
   lines rather than a full list;
3. `F5` expands a normal-buffer command center whose **entire control deck** is
   capped at 40% and replaces the foreground editor with a compact draft
   summary;
4. background events never auto-expand or continuously repaint the collapsed
   strip;
5. foreground and background reply drafts are separate and target-labelled;
6. cross-session permission focus follows §9.2, with no default choice and
   stable run/permission/lease/action identities;
7. short questions and bounded permissions stay inline while retained history,
   long detail, and complex review use alternate screen;
8. attaching a background session must not replay it into native scrollback,
   and any retention gap remains visible;
9. every Code session is supervisor-owned, while clean stop, explicit detach,
   and client loss have different lifecycle outcomes;
10. inline mutations acquire generation-fenced transient leases without
   requiring attach;
11. foreground/background directory claims use the Git worktree root rather
   than only the requested subdirectory;
12. background run permissions default to Ask without changing ordinary
   headless `run`'s deny-all default;
13. daemon-side supervision remains required even though most management is
   inline;
14. only BitRouter-supervised runs are guaranteed visible; native child agents
   require a typed capability; and
15. global usage is omitted unless its scope and attribution are exact.

The approved direction is tracked in the ordered
[implementation plan](BACKGROUND_AGENT_IMPLEMENTATION.md). Work begins with
supervisor ownership and the local attach/event contract, not with the visible
agent strip.

---

## 21. Independent review disposition

The independent GPT-6 Astra review of Revision 2 recommended **approve with
changes**. Revision 3 incorporates all six primary findings and both additional
findings below. This table records specification resolutions, not passing
implementation tests; verification evidence belongs in the implementation
ledger.

| Review finding | Adopted contract | Acceptance criteria |
| --- | --- | --- |
| P1: Cross-session permission focus was ambiguous and the wireframe implied a default approval. | §§7.2, 7.5, and 9.2 define surface-local F2 dispatch, foreground priority, read-only peek, no initial selection, and stable permission/action identities. Buffered input cannot approve a newly focused request. | A4, A11–A13, A22 |
| P1: Foreground ownership and exit disposition were undecided. | §§9.7 and 12.2 put foreground and background controllers under the supervisor from creation, with distinct clean-stop, explicit-detach, and client-loss outcomes. §12.6 includes both in canonical worktree claims. | A16–A17, A23, A32 |
| P1: Inline actions had no lease acquisition path independent of attach. | §§9.5 and 12.5 define transient versus attached leases, acknowledgement-based release, confirmed takeover, generation fencing, and idempotent action IDs. | A20–A21 |
| P2: The 40% height limit excluded unspecified surrounding chrome. | §6.3 and revised wireframes budget the entire expanded control deck, including the preserved-draft summary. At 40×16 the maximum is six rows. Collapse restores client state, not the previous native scrollback viewport. | A5, A8–A10, A15, A36 |
| P2: Bounded retention conflicted with a promise of complete history. | §12.4 separates metadata snapshot from display replay, defines sequence boundaries and replacement resync, pins unresolved actions/unread results, and requires a permanent retention-gap marker. | A14, A33–A34 |
| P2: Background dispatch inherited an unsuitable headless permission default. | D21 and §14.1 make unmatched background requests Ask unless explicitly overridden, preserve ordinary headless deny-all, and define timeout, schema validation, capability-led load/resume, and no-wait conflicts. | A27–A29 |
| Additional: Failed runs had no defined removal path. | §§9.6 and 16.1 allow removal after settled reaping; repeated stop performs cleanup without rewriting failure as success or deleting the native saved session. | A23–A24 |
| Additional: Phase 3 depended on an inspector deferred to Phase 4. | §17 moves the minimum long-permission/detail inspector into Phase 3 and makes it a release prerequisite. Lease takeover race tests belong to Phase 1. | A13, A20–A21 |

No review resolution changes the central UX decision: the foreground document
stays inline, routine background management stays in the bottom control deck,
and alternate screen is an explicit escalation for content that cannot fit
safely or for standalone management.
