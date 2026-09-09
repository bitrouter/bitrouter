# Spec: scrollback-native conversation TUI

Status: **implemented** · Date: 2026-09-09

Baseline: `65891881` (`main`, PR #900 merged after #899 and #901)

Supersedes the rendering decision in PR #900's conversation-first Code spec.

## 1. Decision and review boundary

`bitrouter code` is a coding conversation, not a router dashboard. Its permanent
surface is a transcript in the terminal's native scrollback, a docked composer,
and compact session status. It has no global tabs, sidebar, home page, or
permanent operations panel.

Router and agent capabilities remain available through a searchable command
palette, temporary docked pickers, and explicit read-only inspectors. Large
inspectors use the alternate screen only while the user has deliberately opened
them. Closing an inspector restores the ordinary scrollback surface, exact
draft, cursor, and application focus. The terminal—not BitRouter—owns the
user's native scrollback position.

While a turn is running, the composer is visibly relabelled **Next turn**.
Pressing Enter queues that draft for the next turn. This is FIFO queueing, not
ACP steering, and the UI must never describe it as steering.

The decisions for review are:

| ID | Decision | Consequence |
| --- | --- | --- |
| D1 | One permanent conversation; remove all dashboard tabs. | Home, catalogs, route, requests, policy, telemetry, and reload stop being peer destinations. |
| D2 | The primary surface uses native terminal scrollback. | Do not enter the alternate screen at startup; the terminal owns ordinary transcript scrolling. |
| D3 | Composer, palette, picker, permission, and queue controls share one docked footer. | Temporary interaction adds no permanent chrome and never replaces the conversation. |
| D4 | Large read-only inspectors may temporarily own the alternate screen. | Full transcript, retained tool output, policy, request, telemetry, and similar reports can support search and paging without polluting scrollback. |
| D5 | Enter submits the current composer in both idle and working states. | At idle it starts a turn; while working it queues a visibly labelled **Next turn** draft. |
| D6 | Queued work is explicit FIFO follow-up, not simulated mid-turn steering. | Dispatch only after a normal turn completion. Abnormal outcomes enter `Paused(reason)` and require an explicit **Resume queue**; unresolved permission temporarily blocks dispatch without changing queue order. |
| D7 | Agent, confirmed route, activity, and attributed session cost are the only default session facts. | Other identities and operational detail move to inspectors. |
| D8 | Preserve main's typed local/remote administration actions without preserving its page model. | CLI and TUI remain two front ends over the same action ports and reports. |
| D9 | Preserve main's merged conversation state, editor, ACP lifecycle, permission, cancellation, and restoration contracts. | Replace its full-screen primary renderer rather than discarding its behavior. |
| D10 | In a coding conversation, `Ctrl-O` opens the full transcript inspector by default. | The inspector also exposes stable navigation to retained tool calls, diffs, and other detail entries; an inline **Inspect** action may open a specific entry directly. An operations-only root opens its current status report instead. |

Changes to D1-D10 require explicit design review. Implementation progress and
validation evidence belong in a separate ledger, not in this spec.

## 2. Merged baseline and implemented migration

PR #900 is merged. Current main already removes the eleven-page dashboard,
deletes `crates/bitrouter-tui/src/dashboard.rs`, keeps `apps/bitrouter/src/dashboard.rs`
as a thin entry point, and integrates #901's typed local/remote operations into
Code's command palette and inspectors. It also supplies one conversation state,
multiline editing, searchable commands, explicit FIFO follow-ups, retained
detail, and careful ACP permission/cancellation behavior.

The implementation closes the remaining terminal-ownership and
working-composer mismatch. `CodeView::open()` keeps the normal buffer, the
ordinary conversation has no application-controlled reading anchor, and Enter
queues the visibly labelled **Next turn** draft during work. It reuses the
writer's variable-height `docked_frame()` primitive, changing the primary
renderer and queue contract without redoing the merged information
architecture.

The target combines the merged baseline with the selected interaction model:

```text
main @ 65891881   no tabs, CodeState/editor, ACP correctness, typed operations
main writer       retained journal, live-tail cache, variable-height dock
fx interaction    low-chrome footer grammar and detached detail viewer
──────────────────────────────────────────────────────────────────
implemented       scrollback-native, conversation-first Code TUI
```

This was a renderer and interaction-state migration, not a branch-conflict
resolution. Product state and presentation remain separate, so neither
terminal mode owns network, controller, report, or persistence behavior.

### 2.1 Relationship to existing contracts

This spec supersedes the permanent Code-shell navigation in
[`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md),
the page presentation in [`REMOTE_ADMINISTRATION_SPEC.md`](REMOTE_ADMINISTRATION_SPEC.md),
and the merged PR #900 decision to keep a full-screen primary renderer. It does
not supersede their command naming, ACP lifecycle, native-session ownership,
remote-transport boundary, or action authorization.

It preserves the retained journal and normal-buffer ownership established by
[`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md), with two deliberate extensions:
a variable-height dock and a detached alternate-screen viewer. It preserves
[`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md)'s narrower rule that only
session-safe actions belong in the conversation surface; daemon-wide actions
remain explicit BitRouter-owned operations even when the same palette discovers
them.

## 3. Goals and non-goals

### 3.1 Goals

1. Make conversation the only persistent destination in `bitrouter code`.
2. Preserve the user's ordinary terminal history and native scroll controls.
3. Make every temporary action return to the same draft, application focus, and
   detached-view location; native scrollback position remains terminal-owned.
4. Keep frequent interaction in one low-chrome docked footer.
5. Give large retained content a searchable, pageable, explicitly opened viewer.
6. Make working-state follow-ups convenient without misrepresenting ACP support.
7. Preserve capability-led agent settings, native session identity, permission
   choices, cancellation, and late-update handling.
8. Preserve local and remote router operations through the same typed actions
   used by headless CLI commands.
9. Make source and authority visible whenever model, route, cost, or operation
   scope could otherwise be confused.
10. Remain usable with keyboard-only input, narrow terminals, CJK, emoji,
    multiline paste, resize, suspend/resume, and ordinary shell scrollback.

### 3.2 Non-goals

- A first-party graphical app, fleet dashboard, task sidebar, or multi-session
  workspace.
- A BitRouter-owned transcript database, replacement session ID, or background
  ACP supervisor.
- Remote ACP, remote coding composition, or reconnecting to an already-running
  controller.
- Mid-turn steering unless ACP later exposes a negotiated steering capability.
- Making every CLI leaf a session command.
- A mouse-first interface, configurable widget framework, or general-purpose
  terminal UI toolkit.
- A theme marketplace or user-defined keymap in the first implementation.
- Emitting unbounded reports or full tool output into ordinary conversation
  scrollback.
- Changing router policy, metering, authentication, or action authorization
  semantics as part of the presentation rewrite.

## 4. Surface and terminal ownership

The UI has three presentation layers. Their terminal ownership is part of the
product contract.

### 4.1 Persistent document: native scrollback

The conversation transcript is a vertically rendered document on the terminal's
normal buffer. The terminal owns scrolling. BitRouter may repaint the live tail
that it still owns, but it must not pretend that off-screen native scrollback is
an application-controlled viewport.

The persistent document contains:

- submitted user messages;
- assistant text;
- retained tool and diff summaries;
- short BitRouter-labelled action results when recording them aids continuity;
- completion, interruption, failure, and recovery events.

It does not contain picker rows, permission buttons, palette queries, transient
notices, complete large reports, or every refresh of operational data.

Incoming ACP updates patch the retained journal, which remains authoritative
even after rows have entered native history. The normal-buffer writer tracks a
monotonic commit cursor: which persistent entries have been emitted, which
revision of each entry is still inside the owned live tail, and which rows have
become immutable terminal history. It may patch the owned live tail, but it
cannot rewrite an older tool row after the terminal has committed it to
scrollback. The retained full-transcript/tool inspector shows the newest journal
revision in that case.

BitRouter cannot read or restore the terminal's native scrollback offset. The
primary contract is therefore that it never clears the normal buffer, replays
already committed entries, or deliberately forces a viewport jump. Whether a
terminal follows new output while the user is reading history is controlled by
that terminal's scroll-on-output setting. `Ctrl-L` invalidates and redraws only
the owned live region; it does not clear history, replay the conversation, or
change the draft.

### 4.2 Docked transient surface

The footer is the only persistent interactive region. Depending on state it may
show:

- the ordinary composer;
- the **Next turn** composer and bounded queue preview;
- command palette or slash completion;
- a searchable single-choice picker;
- the oldest pending permission or agent question;
- a short notice or recoverable error;
- compact agent, route, activity, and cost status;
- width-appropriate keyboard hints.

Palette, picker, permission, and queue controls reuse this space. They do not
appear as centered cards and do not establish a second navigation hierarchy.
Opening or closing them must not commit rows to scrollback merely because their
height changed.

### 4.3 Detached read-only inspector: temporary alternate screen

A large inspector may enter the alternate screen only after an explicit user
action. `Ctrl-O` opens the full transcript inspector; its stable entry navigator
can jump to retained tool calls, diffs, and other detail. Accepting an inline
**Inspect** action opens that entry directly. Choosing a large report from the
command palette may also open the inspector. In the operations-only remote root,
where no coding transcript exists, `Ctrl-O` opens the current status report.

Eligible content includes:

- full transcript;
- complete retained tool output and diffs;
- bounded request history;
- model/provider catalogs when they exceed the dock budget;
- policy overview and named policy detail;
- telemetry detail;
- reload receipts and partial-failure detail;
- structured permission context.

The inspector owns paging, search, selection within the report, and an explicit
close control. It is read-only: mutations require returning to the normal
surface and confirming through a docked action or permission flow.

In the full-transcript inspector, `[` and `]` move through retained entries by
stable journal identity. `F4` opens the selected entry's complete detail without
leaving the alternate screen; Escape returns to the same transcript selection
and scroll position. Ordinary arrows and Page Up/Page Down remain report
scrolling, while Ctrl-F searches the current inspector.

The session owns raw mode, keyboard enhancement, bracketed paste, signal hooks,
and the normal-buffer cursor contract for its whole lifetime. A detached
inspector acquires only alternate-buffer custody plus its own cursor
visibility/shape; closing an inspector must not run the session-wide terminal
restore path.

Opening records both the latest journal revision and the normal writer's commit
cursor, then suspends normal-buffer painting. ACP updates, action completions,
metering updates, resize events, and permission requests continue to update
authoritative state in the background. Each new persistent transcript entry is
added to a pending normal-buffer commit range. Revisions to already committed
history update the journal but are not emitted again.

On close, in this order:

1. stop inspector painting and leave the alternate buffer, restoring only the
   cursor state it acquired;
2. resume the normal buffer with session-owned input modes still active;
3. emit every persistent entry after the recorded commit cursor in document
   order, exactly once, even when the backlog exceeds several screens;
4. apply current revisions to entries that remain in the owned live tail,
   invalidate its cache, and draw the current dock;
5. advance the commit cursor atomically so retrying restoration cannot duplicate
   the backlog;
6. restore exact draft bytes, grapheme cursor, prior dock focus, and any detached
   search/navigation state retained for reopening;
7. show newly arrived permission, failure, or disconnect notices at normal
   priority without transferring input focus implicitly.

The restore path must handle a backlog larger than three terminal screens, an
old tool entry receiving a late update, and one or more resizes while detached.
It never replays the whole journal and never claims to restore the terminal's
native scrollback offset.

An inspector may navigate within one alternate-screen session, but inspectors
must not nest alternate buffers. On SIGTSTP the process restores all acquired
terminal modes before suspension; on SIGCONT it reacquires session modes and
redraws whichever normal or detached surface is authoritative. SIGINT, SIGTERM,
panics, ordinary exit, and failed external-editor handoff likewise restore every
mode the process acquired exactly once.

## 5. Persistent layout and visual grammar

Illustrative 80-column state; values are examples:

```text
𝒃 bitrouter · project

┃ Keep the selected route when I reopen the picker.

  ● Read 4 files
  ● Ran cargo nextest run -p bitrouter-tui
  ● Edited picker.rs · +8 −4                         Inspect
  • Running focused checks (12s)

┋ Queued next · Also check CJK model names.

┋ Next turn
┋ Continue typing…

Codex · route coding-default · working 12s · USD 0.0800 router
Ctrl-P Commands · Ctrl-O Details
```

Presentation rules:

- Use typography, indentation, weight, and semantic glyphs before borders.
- Submitted user messages and the idle composer use a strong `┃` rail.
- The working composer and queued next-turn previews use a dim/dotted `┋` rail;
  the editable composer also carries a visible **Next turn** label.
- Tool/activity rows use stable meanings such as `● Read`, `● Ran`, `● Edited`,
  `■ Cancelled`, and `✓ Recovered`.
- Do not say **Thinking** unless the agent reports that semantic state. When the
  client knows only that a turn is active, say **Working**.
- Ordinary assistant text has no card or bubble.
- Ordinary notices have no titled box. Use a border only when it expresses a
  real boundary, such as the start of a temporary picker.
- Keep the palette and pickers inline below the composer, with `›` for the
  selected row and aligned annotations where width permits.
- Use a mostly neutral palette. Reserve semantic color for diffs, destructive or
  dangerous states, errors, and active selection. Every meaning also has a text
  or glyph distinction.
- Put the short workspace folder in the terminal title or welcome line, not the
  permanent status. Do not permanently show full paths, branch, daemon PID,
  listener, session ID, provider catalog, or configuration inventory.

Startup welcome is at most two lines and disappears into scrollback. An empty
conversation is the home surface; there is no separate Home screen.

## 6. Navigation and discovery without tabs

`Ctrl-P` opens the searchable full action inventory. Typing `/` at the beginning
of a draft opens the slash-capable subset plus agent commands and prompt
templates. Directly typed commands use the same resolver and precedence as
their palette counterparts. A palette-only action need not have a slash spelling.

Rows identify their owner:

- **BitRouter** for typed local/remote actions;
- **Agent** for commands advertised by the active ACP session;
- **Prompt template** for local user-authored expansion.

Local BitRouter commands shadow same-named agent commands, and discovery must
make that ownership visible rather than silently selecting one.

The merged palette is the naming baseline. A label is not automatically a slash
command, and this spec does not authorize new aliases:

| Capability | Current palette label | Current slash entry | Target surface |
| --- | --- | --- | --- |
| Home | None; the empty conversation is home | None | Persistent conversation |
| Agent selection | **Choose agent** | None | Docked searchable picker |
| Native sessions | **Open session** | None | Docked capability-led picker |
| Agent configuration | **Agent settings** | Agent-advertised commands only | Docked setting picker |
| Routable catalog | **Routable models** | `/models` | Detached catalog inspector |
| Host requests | **Host requests** | None | Detached bounded inspector |
| Session route | Agent/session route control | `/route`, `/route reset` | Docked session-route picker/action |
| Route preview | **Route preview** | `/preview` | Docked model selector, then detached detail |
| Providers | **Providers** | None | Detached read-only inspector |
| Telemetry | **Telemetry** | None | Detached read-only inspector |
| Policy | **Policy status**, **Policy detail** | None | Detached inspector; named detail starts with a docked selector |
| Agent catalog | **Agent catalog** | None | Detached read-only inspector |
| Reload | **Reload state**, **Reload now** | None | Detached state/receipt; mutation starts with docked confirmation |

Any future `/agent`, `/session`, `/requests`, `/providers`, `/telemetry`,
`/policy`, or `/reload` alias is a separate public command-inventory decision.
It must define precedence, update the shared inventory and shipped skill, and
receive its own compatibility review. `/preview` remains distinct because
`/route` already owns session-route selection.

Closing a palette or picker restores its exact query only when returning from a
recoverable failure that originated there. Ordinary dismissal restores the
conversation draft, cursor, and prior application focus. It makes no claim
about the terminal's native scrollback offset.

Agent model setting, session route override, routable catalog, and latest
observed upstream are different facts. The UI must not collapse them into one
generic **Model** field or imply that changing one changed the others.

## 7. Composer and next-turn queue

### 7.1 Editing contract

The composer preserves PR #900's editor behavior:

- grapheme-safe left/right and word movement;
- Home/End and deletion;
- multiline editing;
- process-local prompt history at logical first/last lines;
- bracketed paste with exact line breaks and no implicit submit;
- `Shift-Enter` or `Alt-Enter` for newline where supported, with `Ctrl-J` as the
  documented fallback;
- deliberate `$VISUAL`/`$EDITOR` handoff at safe idle points;
- draft and cursor preservation across picker, inspector, failure, resize,
  suspend/resume, and reconnect preparation.

### 7.2 Enter semantics by state

| State | Composer label | Enter | Escape |
| --- | --- | --- | --- |
| No agent selected | **Message** | Open agent picker; retain draft for explicit later submission | Close the picker if open; otherwise leave the draft unchanged |
| Idle, connected | **Message** | Submit one turn and clear the submitted draft | Close the focused transient surface; otherwise leave the draft unchanged |
| Submitting | **Next turn** | Move the exact draft to pending follow-ups; queue it only after the original turn is accepted | Request cancellation through the explicit interrupt path; preserve the next-turn draft and pending follow-ups |
| Working | **Next turn** | Append the exact draft to the FIFO queue and clear the composer | Interrupt the active turn; preserve but do not queue the draft |
| Cancelling | **Next turn — waiting for stop** | Do not dispatch; retain the draft and explain that cancellation is settling | No second cancellation side effect |
| Queue paused, connected | **Next turn — queue paused** | Append the exact draft to the end of the paused queue; do not send it | Leave the draft unchanged |
| Permission/question focused | No composer ownership | Confirm only an explicitly selected option | Deny/cancel according to the protocol-specific contract |
| Disconnected/failed | **Message — disconnected** | Retain the draft and open recovery choices; never pretend it was submitted | Close recovery surface without losing draft |

Enter always means “submit the currently labelled composer.” At idle its target
is the active agent turn. While work is active its target is the visible
**Next turn** FIFO. This consistency replaces PR #900's `Tab`-to-queue shortcut.

The working composer must not use copy such as **Steer**, **Update current
turn**, or **Send now**. A future ACP steering capability may introduce a
separate negotiated action; it must not silently change this queue contract.

### 7.3 Queue behavior

- Prompt text has separate ownership buckets: the prompt awaiting submission
  acceptance, the active in-flight prompt, pending follow-ups entered during
  submission, queued items, the one queue item being dispatched, recovery
  drafts, and the currently editable composer draft. Moving text between these
  buckets is explicit; no transition clears or overwrites a different bucket.
- Queue items contain exact prompt text and visible origin where relevant.
- Submitting an idle draft `P0` moves it out of the editor into the submission
  slot. The user may type `P1`; Enter moves `P1` to pending follow-ups and leaves
  the editor available for `P2`.
- If `P0` is accepted, it becomes the in-flight prompt and pending follow-ups
  enter the FIFO in order. Any unsubmitted `P2` remains in the editor.
- If `P0` is rejected, `P0` and every pending follow-up move, in order, to an
  explicit **Recover drafts** surface. The current editor draft remains intact.
  Nothing is retried, merged, or silently promoted. Restoring one recovery item
  to the editor requires an explicit choice and cannot overwrite a non-empty
  editor.
- Show at most the first two queued previews in the dock; indicate the remaining
  count and offer an explicit queue editor.
- The queue editor can inspect, edit, remove, or reorder only through explicit
  focused actions. Closing it changes nothing.
- Dispatch one item only after the previous turn completes normally and no
  permission remains unresolved.
- Automatic dispatch moves the oldest item into a dedicated dispatch slot. It
  never reads, clears, replaces, or submits the current editor draft. The item
  leaves the slot only after acceptance; rejection moves it to recovery and
  pauses the remaining queue.
- Cancellation, abnormal stop, submission failure, disconnect, session
  replacement, or unavailable agent command changes the queue to
  `Paused(reason)`. Resolving a permission merely unblocks the active turn; it
  does not pause, resume, or reorder the queue.
- A paused queue never resumes because time passed, a connection reappeared, a
  permission resolved, or the user submitted another draft. **Resume queue** is
  an explicit action and is enabled only for the same connected agent/native
  session with no unresolved permission or uncertain submission.
- While paused, Enter appends the labelled **Next turn — queue paused** draft to
  the end. It cannot overtake older work. To send it first, the user must open
  the queue editor and explicitly reorder or discard older entries before
  choosing **Resume queue**.
- Never replay an uncertain submission after reconnect.
- Never transfer queued work to a different agent or native session. Changing
  either requires dispatching, moving items to recovery, or discarding them.
- BitRouter administration actions cannot be queued as agent prompts.
- A queued item that resolves to an agent command is revalidated against the
  currently advertised command inventory immediately before dispatch.

## 8. Status and provenance

The default session status contains four facts, in priority order:

| Field | Source and wording |
| --- | --- |
| Agent | Resolved agent identity and lifecycle; catalog availability is not connection. |
| Route | Confirmed session override or known default; distinguish no override, direct, and unreported. |
| Activity | Client-observed lifecycle or reported tool label; no invented percentage or thinking state. |
| Session cost | Cumulative ACP session cost with router-attributed, agent-reported, unknown, or unreported provenance. |

Permission-needed, failure, disconnect, and cancellation outrank ordinary
activity. Context used/size appears in details when the agent reports a usable
pair; it is not a fifth always-visible field.

Do not equate:

- agent model setting with session route;
- requested route with actual upstream;
- host-wide spend with session cost;
- absence of cost with zero;
- a cached catalog row with a connected process.

At narrow widths preserve state meaning and provenance before optional names,
elapsed time, hints, or branding. The status may wrap to two lines at 80×24 and
below; it must not crowd out a permission choice or one usable composer row.

## 9. Permission, questions, and interruption

Permission requests are queued by stable identity and resolved exactly once.
Arrival raises a prominent **Permission needed · F2 to review** notice but does
not steal composer, palette, picker, or inspector input ownership. `F2` is the
only ordinary transition into permission focus. The transition consumes that
keypress, does not replay buffered input from the prior surface, and clears any
previous option highlight. The oldest unresolved request then owns the dock. No
option starts selected; Enter without a post-focus explicit selection has no
side effect.

While a permission is pending, ordinary report and catalog inspectors cannot be
opened. The only allowed detached transition is the permission's own structured
context inspector, which closes back to the same unresolved request.

A pending permission outranks ordinary status, but draft bytes remain intact
and editing may continue until the user presses `F2`. If a permission arrives
while a detached inspector is open, the inspector shows a bounded **Permission
needed · F2 to review** indicator. `F2` closes the inspector and explicitly
focuses the oldest request. Closing the inspector normally only returns to the
normal surface and repeats the notice; it never grants permission input
ownership. The alternate inspector never renders mutation controls over report
content.

Cancellation and permission rejection remain different outcomes:

- Escape during working requests turn cancellation.
- Cancelling a turn does not approve, reject, or discard a pending permission
  unless the ACP teardown contract requires an explicit cancellation response.
- Closing a permission surface uses the protocol-defined cancellation/deny
  outcome, not the generic turn-cancel path.
- Late ACP updates are consumed until the original prompt settles or a bounded
  failure proves it cannot settle.
- Queued next-turn work stays paused after abnormal termination.

## 10. Local and remote operations

The implementation preserves #901's typed reports, authorization, target
resolution, refresh provenance, retained reload receipts, and no-local-fallback
rules. It removes their permanent dashboard pages.

For a local ACP session, router operations appear in the command palette. Short
results may add one BitRouter-labelled transcript event; large results open a
detached inspector. A mutation such as reload requires a docked confirmation
that states target, authority, and effect before the action starts.

For a named remote context, remote ACP remains unavailable. The root interactive
surface is therefore **Remote operations**, not a fake conversation:

```text
𝒃 bitrouter · production

Remote operations · reports read-only · reload explicit
Last refresh 12s ago · connected

No coding agent is attached to this context.

Ctrl-P Commands · Ctrl-O Status details · Ctrl-C Exit
```

This root stays on the normal buffer and has no enabled coding composer. Its
palette exposes only capability-confirmed remote actions. Read actions open
temporary inspectors. Reload appears only with explicit `control:reload`
authority and follows the same confirmation and retained-receipt contract as
the CLI. Remote failures never fall back to local files, sockets, config,
metering, or agent catalogs.

## 11. State and rendering boundary

The pure interaction state from PR #900 remains the behavioral owner. The exact
Rust shape may change, but the concepts remain separate:

```rust
struct CodeState {
    journal: Journal,
    editor: Editor,
    turn: TurnState,
    dock: DockSurface,
    submission: Option<PendingSubmission>,
    queue: QueueState,
    recovery: VecDeque<RecoveryDraft>,
    permissions: VecDeque<PendingPermission>,
    permission_focus: Option<PermissionId>,
    status: CodeStatus,
}

struct QueueState {
    items: VecDeque<QueuedPrompt>,
    dispatching: Option<QueuedPrompt>,
    state: RunningOrPaused,
}

enum DockSurface {
    Composer,
    Palette(ChoiceList),
    Selector(ChoiceList),
    Permission,
    QueueEditor,
}

enum DetachedSurface {
    FullTranscript,
    ToolDetail(ToolCallId),
    Report(Inspector),
}

struct PrimaryRenderState {
    committed: NormalBufferCommitCursor,
    live_tail: LiveTailCache,
}

struct DetachedViewState {
    opened_at: JournalRevision,
    normal_commit_at_open: NormalBufferCommitCursor,
    search_and_navigation: DetachedNavigation,
}
```

This is a contract sketch, not permission to introduce unused abstractions.
Detached viewer state may live beside the renderer if it contains only paging,
search, and terminal presentation state. Product state, action authority, ACP
effects, and reports remain outside the renderer.

`NormalBufferCommitCursor` is application bookkeeping, not a terminal viewport
or scroll offset. Queue dispatch must use its dedicated prompt slot; current
main's editor-clearing `begin_prompt()` behavior cannot be reused for automatic
dispatch unless it is split so an unrelated editor draft is preserved.

The reducer is synchronous and emits plain effects. The application owns ACP
I/O, target resolution, report fetching, controller lifecycle, clipboard,
external editor, and async work. The renderer owns terminal modes, layout,
painting, and cursor placement. Neither renderer may read router config, HTTP,
IPC, metering storage, or credentials directly.

## 12. Integration with current source

The implementation follows merged main's existing ownership boundaries:

| Source | Keep | Replace or remove |
| --- | --- | --- |
| `crates/bitrouter-tui/src/writer.rs` | Differential normal-buffer writer, live-tail cache, terminal restoration, existing variable-height `docked_frame()` | Assumption that the entire Code document is an application viewport |
| `crates/bitrouter-tui/src/code.rs` | `CodeState`, editor, dock surfaces, retained journal, selectors, permission identities, queue editor | Startup alternate-screen ownership, primary `ReadingAnchor`, Tab-to-queue semantics |
| `apps/bitrouter/src/chat/code.rs` and wire | Reducer/effect boundary, ACP update consumption, lifecycle, cancellation, capability negotiation | Editor-clearing automatic queue dispatch; implicit focus changes during detached restoration |
| `apps/bitrouter/src/actions/code.rs` and administration target | Typed local/remote reports, authorization, target isolation, reload scope and receipts | Hand-authored presentation assumptions; action behavior remains unchanged |
| thin `apps/bitrouter/src/dashboard.rs` entry | Argument/target assembly into the shared Code loop | Nothing; the old page driver and `crates/bitrouter-tui/src/dashboard.rs` are already gone |
| shared action inventories plus merged palette labels | Existing command IDs, slash names, reports, requirements, ownership and availability | Invented aliases or a second palette registry |

No CLI flag, command, default, harness wiring, or plugin invocation changes are
authorized by this spec alone. If implementation changes those public surfaces,
update `skills/bitrouter/` and the agent-plugin manifests in the same change.

## 13. Responsive, input, and accessibility contract

- At 80×24, a submitted user turn, one active-status row, one composer row, and
  essential hints remain readable without clipping.
- At 40×16, preserve one usable composer row, the full meaning of a permission
  choice, and status provenance before optional labels.
- For terminals at least 40×16, the dock content budget is
  `clamp(floor(rows × 0.4), 5, 12)` rows, excluding the compact status/hint row.
  Composer and choice lists scroll inside that budget. Read-only content whose
  wrapped body exceeds the budget opens detached; full transcript, tool/diff
  detail, request history, policy detail, and reload receipts are always
  detached even when currently short.
- Below 40×16, show a minimal resize surface while retaining journal, draft,
  queue, permission, and selection state. Resize cannot approve or submit. If a
  permission is pending, its identity plus **F2 Review · Esc Deny · Ctrl-C
  Cancel turn** remain available; approval is disabled until the offered choice
  can be rendered completely.
- All clipping and wrapping use display width and grapheme boundaries, not byte
  or scalar counts.
- Sanitize terminal control bytes from agent, tool, report, file, provider,
  model, branch, and error text before measuring or painting.
- Bracketed paste never submits. Newline key fallbacks remain visible at widths
  where the primary shortcut cannot be represented reliably.
- Light and dark terminal backgrounds receive readable contrast. Color is never
  the only indication of owner, status, permission, diff direction, or error.
- The physical terminal cursor remains at the actual composer position.
- Primary operation is keyboard-only. Mouse support is outside this release.
- The supported contract is a VT-compatible terminal with normal/alternate
  buffers, cursor save/restore, raw input, and bracketed paste. Automated PTY
  tests use the repository's VT parser; release smoke tests cover macOS Terminal,
  iTerm2, and WezTerm. Scroll-on-output preferences may differ, so tests assert
  no clear/replay/forced jump rather than claiming control of terminal history.

## 14. Acceptance criteria

| ID | Required evidence |
| --- | --- |
| A1 | Starting `bitrouter code` leaves the terminal on the normal buffer and preserves native scrollback. |
| A2 | The ordinary Code UI contains no global `Page` navigation, tabs, sidebar, or Home screen. |
| A3 | Submitted user, assistant, tool, diff, and completion entries remain correctly ordered in the retained journal; the owned live tail is patchable, while immutable native history is never falsely rewritten or duplicated. |
| A4 | Opening and dismissing palette, picker, permission, and queue surfaces restores exact draft bytes, grapheme cursor, and application focus; no test claims control of the terminal's native scroll offset. |
| A5 | Dock height changes do not commit picker/permission rows to scrollback or replay the transcript. |
| A6 | Ctrl-P exposes the authoritative full action inventory; leading `/` exposes its slash-capable subset plus live agent commands and prompt templates. Shared actions have identical ownership, availability, resolver precedence, and naming, with no fabricated aliases. |
| A7 | At idle, Enter submits exactly one prompt. During submission/work, a visible **Next turn** composer makes Enter retain exactly one pending/FIFO follow-up without overwriting a newer editor draft. |
| A8 | No queue path or copy claims mid-turn steering; automatic dispatch waits for normal completion, never clears the editor, and abnormal outcomes enter `Paused(reason)` until explicit **Resume queue**. |
| A9 | Queue edit/remove/reorder/resume is explicit, preserves exact prompt text, prevents new drafts overtaking paused work, and cannot transfer items to another agent/session. |
| A10 | Agent model setting, route override, routable catalog, and observed upstream remain separately labelled. |
| A11 | Agent, route, activity, and cost render with honest unknown/unreported/provenance states and responsive priority. |
| A12 | Permission arrival never steals input. F2 explicitly focuses the oldest request with no selected option; only subsequent selection plus Enter confirms, and overlapping requests are retained and resolved once. |
| A13 | Full transcript and large reports enter the alternate screen only after explicit user action and remain read-only. |
| A14 | ACP updates, permissions, action results, metering, and resize events continue to update authoritative state while an inspector is open; normal-buffer painting stays suspended. |
| A15 | Closing after more than three screens of background output commits only new persistent entries, in order and exactly once; late updates to immutable old entries appear in retained detail without replay. |
| A16 | The session and detached inspector restore only the terminal modes they own; SIGINT, SIGTERM, panic, ordinary exit, resize, SIGTSTP/SIGCONT, and external-editor failure are idempotently covered. |
| A17 | Local and remote operational commands use #901's typed action ports, authorization, redaction, target isolation, and retained receipts. |
| A18 | Remote operations show no coding composer and never fall back to local state or imply remote ACP. |
| A19 | Real-PTY journeys cover 80×24, 40×16, below-minimum safe permission exit, CJK/emoji, exact multiline paste, queueing, permission during inspector, cancellation, disconnect, resize while detached, and alternate-screen restoration. |
| A20 | Headless text/JSON/quiet behavior and existing ACP/controller safety tests remain unchanged. |
| A21 | A `P0` submission with pending `P1` and editable `P2` preserves all three exactly on both acceptance and rejection; rejection exposes ordered recovery without implicit retry. |
| A22 | Automatic dispatch of queued `P1` while editable `P2` exists leaves `P2` byte-for-byte unchanged; dispatch rejection pauses the queue and preserves the item for recovery. |
| A23 | Buffered arrows, digits, or Enter from a composer/inspector cannot authorize a newly arrived permission; ordinary inspector close returns to a notice, while F2 performs the explicit focus transfer. |

## 15. Delivery sequence

1. **Make the primary renderer normal-buffer-native:** retain the merged reducer
   and `docked_frame()`, remove startup alternate-screen ownership, and give the
   writer an explicit normal-buffer commit cursor/live-tail boundary.
2. **Define detached catch-up:** suspend normal-buffer painting while detached,
   record journal/commit revisions, then implement ordered exactly-once backlog
   commit and late-update behavior across resize and restoration.
3. **Implement the complete Next turn state machine:** replace Tab queueing,
   separate submission/in-flight/editor/pending/queue/dispatch/recovery text,
   add `Paused(reason)` and explicit **Resume queue**, and preserve ordering.
4. **Split terminal custody:** keep raw/input modes session-owned and make the
   inspector own only alternate-buffer/cursor presentation. Add idempotent
   SIGTSTP/SIGCONT, signal, panic, and external-editor transitions.
5. **Hold the permission focus boundary:** retain notice-only arrival, F2-only
   focus transfer, empty initial selection, and no buffered-key replay across
   normal/detached surfaces.
6. **Finish inspector and visual grammar:** make Ctrl-O open the full transcript
   with stable detail navigation; apply rails, semantic activity markers,
   neutral hierarchy, compact status, dock limits, and width-degrading hints.
7. **Validate end to end:** add the P0/P1/P2, paused-ordering, multi-screen
   detached backlog, immutable late-update, resize, permission-key isolation,
   and terminal-custody tests before all-feature and real-PTY validation.

The no-tabs information architecture and typed #901 operations are baseline,
not delivery work. Each step must leave one terminal owner and one interaction
reducer. Do not keep full-screen and scrollback Code implementations as
selectable modes merely to reduce migration difficulty.

## 16. Rejected alternatives and the case against this design

### 16.1 Rejected: full-screen conversation as the primary UI

This is the merged main renderer inherited from PR #900. It gives complete
viewport control and makes overlays straightforward, but discards native
terminal history, makes a
conversation feel like an application dashboard, and couples ordinary reading
to BitRouter's scrolling model. The user selected native scrollback instead.

### 16.2 Rejected: retain dashboard tabs and improve their styling

Tabs preserve discoverability, especially for remote administration, but encode
the wrong information architecture. Conversation, selecting an agent, and
reading policy are not equal-duration destinations. Eleven permanent tabs also
consume scarce width and force session-scoped and daemon-wide concepts into one
navigation layer.

### 16.3 Rejected: prohibit alternate screen everywhere

This preserves one terminal mode but leaves no clean home for large searchable
reports or full retained output. Dumping them into scrollback permanently
pollutes conversation; forcing an external pager fragments state and restoration.
An explicitly opened, read-only, temporary alternate-screen viewer keeps the
primary experience shell-native without denying deep inspection.

### 16.4 Rejected: Tab queues the next turn

It is explicit but conflicts with completion/focus expectations and makes Enter
mean “submit” only in one lifecycle state. Relabelling the working composer
**Next turn** lets Enter retain one visible meaning: submit to the named target.

### 16.5 Rejected: imitate fx steering copy over a local queue

The current ACP surface does not advertise true mid-turn steering. A local FIFO
changes when work runs, not what the active model sees. Calling it steering
would be a protocol and UX falsehood.

### 16.6 Rejected: emit every inspector into conversation scrollback

This makes details durable but mixes conversation history with repeatedly
refreshed host-wide state, policy catalogs, and request tables. It also makes
closing impossible: scrollback cannot retract what has already been emitted.

### 16.7 The case against the selected design

The selected design is harder to implement than retaining the current merged
full-screen renderer:

- a variable-height dock over native scrollback needs precise live-region and
  cursor ownership;
- background updates during an alternate-screen inspector require an
  authoritative journal, a normal-buffer commit cursor, and reliable
  exactly-once restoration;
- removing tabs reduces passive discoverability and puts more pressure on the
  palette, slash completion, welcome copy, and contextual hints;
- Enter changes destination between **Message** and **Next turn**, so the state
  label must be unmistakable;
- a mostly neutral visual system can under-signal risk unless permission,
  fallback, unpriced cost, partial reload, and disconnection retain explicit
  textual distinctions;
- terminal custody, queue text ownership, and permission focus must be explicit
  across more transitions than the full-screen implementation needs.

These costs are accepted because they concentrate complexity in state and
rendering correctness while leaving the daily product surface simpler and more
honest.

## 17. References

- [PR #900: conversation-first Code TUI](https://github.com/bitrouter/bitrouter/pull/900)
- [PR #901: remote router administration](https://github.com/bitrouter/bitrouter/pull/901)
- [`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md)
- [`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md)
- [`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md)
- [`REMOTE_ADMINISTRATION_SPEC.md`](REMOTE_ADMINISTRATION_SPEC.md)
- [fx](https://github.com/vercel-labs/fx)
