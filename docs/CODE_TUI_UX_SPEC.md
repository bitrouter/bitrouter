# Spec: conversation-first Code TUI

Status: **implemented; local acceptance verification complete**

Tracking: [CODE_TUI_UX_PROGRESS.md](CODE_TUI_UX_PROGRESS.md).

Date: 2026-09-08

Source baseline: `b657cb62` (`v1.0.0-alpha.30` release commit)

## 1. Decision and review boundary

`bitrouter code` is a coding conversation. Its persistent interface is the
transcript, composer, and a compact status area. Remove the seven-view
navigation: Home, Agents, Conversation, Sessions, Models, Requests, and Route.
Expose their useful capabilities as temporary pickers, inspectors, and bounded
actions that return users to the same conversation.

The maintainer accepted the conversation-first direction and the proposed
always-visible information: **agent, route, activity, and attributed session
cost**. This document makes that direction concrete for review. The remaining
choices below define the implementation the maintainer subsequently authorized.

| Decision | Proposed contract |
| --- | --- |
| D1: primary surface | One conversation; no permanent page tabs or sidebar |
| D2: rendering | Retain full-screen rendering for the first implementation; do not couple this work to an inline-renderer rewrite |
| D3: discovery | Searchable command palette and slash completion, plus contextual hints |
| D4: composition | Editable multiline draft, intact paste, history, and drafting during a running turn |
| D5: follow-ups | Explicit local queue for the next turn; no simulated native mid-turn steering |
| D6: routing | Session route controls and agent model settings remain separate, clearly labelled operations |
| D7: sessions | One active native session; capability-led opening; no BitRouter transcript database |
| D8: operations | On-demand inspectors using existing typed reports; retain remote read-only access without remote ACP |

Changes to these decisions must remain explicit during implementation review.

### Review guide

Start with the old-view mapping in §5.2 and the proposed terminal layout in
§6. The central choice is to make conversation the permanent destination,
with controls returning to the same draft and reading position.

The recommendations that most affect day-to-day use are:

- Keep full-screen rendering for this release while removing permanent tabs.
- Show agent, confirmed route, activity, and attributed session cost; move
  catalogs and operational detail into temporary inspectors.
- Allow drafting during work, with an explicit next-turn queue. Do not claim
  native mid-turn steering through ACP.
- Derive agent commands, permissions, settings, and session controls from
  advertised protocol support; separate them from BitRouter-owned actions.
- Preserve remote operations as a visibly read-only inspector without a coding
  composer until remote ACP exists.

The delivery sequence and A1–A14 acceptance criteria in §12 define completion.
The linked implementation ledger records the completed checks, acceptance
evidence, and limits of native-adapter validation.

## 2. Problem and source evidence

The current navigation treats different activities as equal destinations:
conversation is continuous work; selecting a model is a brief decision; reading
request history is an occasional investigation. The tab bar makes users leave
the work to operate its controls and mixes native-session and daemon-wide scope.

The source audit also found two independent conversation drivers. They share
the journal/renderers but have different input, command, permission, and status
behavior. Removing tabs alone would leave these inconsistencies intact.

| Component at the source baseline | Observed behavior |
| --- | --- |
| [baseline `dashboard.rs` renderer](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/dashboard.rs) | Seven permanent tabs; full-screen permission heading says choose 1–9 without rendering the offered option labels; conversation status omits the inline cost/context footer |
| [baseline `dashboard.rs` driver](https://github.com/bitrouter/bitrouter/blob/b657cb62/apps/bitrouter/src/dashboard.rs) | Incoming updates reset scroll; one pending-permission slot; conversation prompts bypass local action resolution; ordinary typing is ignored while a turn runs |
| [baseline `editor.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/editor.rs) | Inline editor has no cursor movement or history and flattens multiline paste |
| [baseline `machine.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/machine.rs) | Inline reducer has explicit phases, queued permissions, and local slash resolution that the dashboard does not share |
| [baseline `view.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/view.rs) and [`cost.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/cost.rs) | Existing cost attribution, route, and session metadata presentation to preserve |
| [baseline `render/mod.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/crates/bitrouter-tui/src/render/mod.rs) | Retained structured tools and diffs; mostly generic presentation; expansion deferred |
| [baseline `acp_cli.rs`](https://github.com/bitrouter/bitrouter/blob/b657cb62/apps/bitrouter/src/acp_cli.rs) | Shared agent resolution, session host, new/load/resume selection, and negotiated capability snapshot |

These are source-level findings, not measured latency or usability results.
Historical spec status labels are not proof of current implementation.

## 3. Relationship to existing contracts

This design supersedes the permanent Code-shell navigation in
[`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md)
and the dashboard presentation in
[`REMOTE_CONTROL_MVP_SPEC.md`](REMOTE_CONTROL_MVP_SPEC.md). It preserves their
public command naming, native-agent ownership, and remote HTTP action boundary.

It preserves the controller topology in
[`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md), and the shared action/port
boundary in [`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) and
[`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md). It replaces presentation
restrictions in [`TUI_RENDERER_SPEC.md`](TUI_RENDERER_SPEC.md) only where needed
for the composer, inspection, and explicit scroll ownership described here.

Cancellation requires a narrow amendment to
[`ACP_SAFETY_INVARIANTS.md`](ACP_SAFETY_INVARIANTS.md): rejecting an operation and
cancelling a turn are distinct outcomes (§9). Do not preserve reject-on-cancel
solely because existing tests encode it, or weaken deny-on-abandon behavior for
unrelated cases.

Remote ACP remains deferred under
[`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md). No transport,
supervisor, credential, or provider integration change is implied by this UI.

## 4. Scope

The first implementation includes navigation replacement, one shared
interaction core, the four-field status contract, useful composition, explicit
follow-up queueing, permission/cancellation correctness, and transcript
inspection. It preserves existing local and remote operational capabilities.

Non-goals:

- A first-party graphical application, fleet dashboard, or persistent sidebar.
- A BitRouter-owned durable transcript/session catalog or background supervisor.
- Cross-agent transfer of context, automatic agent switching, or multi-agent
  orchestration.
- Universal Codex/Claude/OpenCode native-feature parity through ACP.
- A new inline/full-screen preference or a new widget framework.
- New remote ACP, terminal-tool, filesystem-tool, or authentication capabilities.
- New billing estimates, routing decisions, or inferred per-request attribution.
- Native rewind/undo, fork/delete UI, image/audio composition, shell escape
  execution, or configurable keymap/theme systems in this release.

## 5. Entry, navigation, and restoration

### 5.1 Local entry

- `bitrouter code <agent>` resolves the existing agent identity and opens the
  requested new/load/resume session directly into the conversation.
- Bare `bitrouter code` shows the empty conversation with a searchable **Choose
  agent** picker. It does not launch an arbitrary first agent or introduce a
  remembered-agent preference. Cancelling the picker leaves the empty composer.
- A prompt written before connection is retained. Sending it opens the picker;
  after connection the prompt stays a draft until explicitly submitted.
- A supplied invalid agent or unsupported lifecycle flag remains an explicit
  error. Do not silently create a new session when load/resume was requested.
- Show connection/authentication progress where activity normally appears. A
  failed connection offers the existing actionable diagnostic and retry/change
  agent actions; it must not erase the draft or pretend to be connected.
- Keep terminal authentication disabled unless a supported terminal handoff is
  implemented separately. Provide the existing external authentication guidance.

### 5.2 Where the old views go

| Old view | Replacement |
| --- | --- |
| Home | Empty conversation with project context, prompt, and command hints |
| Agents | Choose agent picker, scoped to available ACP facets |
| Conversation | Primary surface |
| Sessions | Open session picker when listing is supported; enter a native ID when only load/resume is supported |
| Models | Agent settings picker and a separate routable-model catalog inspector |
| Requests | Bounded request inspector labelled by its actual scope; detailed history remains available through `bitrouter requests` |
| Route | Session route picker and read-only route preview/inspection |

Changing agents creates or opens that agent's own session. It never relabels
the current transcript as belonging to another agent. During a running turn,
agent/session replacement is unavailable until the turn settles or cancellation
completes. Preserve the existing session on picker dismissal. If a replacement
requires shutting down the connection before opening another, retain the old
view on failure as a clearly disconnected view with its native ID and draft;
do not imply that the old session is still attached.

### 5.3 Temporary surfaces

There are three surface types: a searchable single-choice picker, a read-only
inspector, and a permission/question surface. Closing a picker or inspector
restores draft text, cursor, transcript anchor, and prior focus. Pending updates
continue to be consumed while these surfaces are open.

Inspectors may occupy the whole terminal for long output. Their title and exit
hint identify what is being inspected. They are temporary views of the active
work, without a global page bar. A brief action result may be recorded as a
BitRouter-labelled transcript event; large reports must not repeatedly flood it.

### 5.4 Remote read-only entry

Existing `bitrouter --context <name> code` remains useful for remote operations.
Since remote ACP is unavailable, open a labelled **Remote operations** status
inspector with the same command palette offering supported status, models,
request history, and route preview actions. No seven-tab dashboard returns.

The remote inspector displays the selected target and read-only scope. It has
no enabled coding composer, agent launcher, or session route mutation. Closing
its root inspector exits; closing a nested inspector returns to its root.
The four session status fields apply only when a local ACP session exists.
Remote errors never fall back to local data. Existing local `--socket`-only
operational use follows the same inspector pattern and existing argument guards.

## 6. Persistent layout and status

Illustrative layout; values are examples, not measured results:

```text
bitrouter code · ~/work/project

› Fix model-name search in the route picker.

I'll inspect the picker, update selection, and check the behavior.
✓ Explored 3 files · 2 searches                  [inspect]
✓ Edited picker.rs · +8 -4                       [inspect]
◍ Running focused checks

Next turn: Also check narrow terminals.          [edit/remove]
────────────────────────────────────────────────────────────
› Keep the current route selected when I reopen the picker.
────────────────────────────────────────────────────────────
agent: Codex · route: coding-default
activity: working 12s · session cost: USD 0.0800 (router)
Tab queue next · Esc interrupt · Ctrl-P commands
```

Do not permanently show session IDs, provider catalogs, daemon PID/listeners,
all configuration options, or a task-management sidebar.

### 6.1 The four fields

| Field | Meaning and source |
| --- | --- |
| Agent | Resolved selected agent identity; connected state comes from lifecycle, not catalog availability |
| Route | Confirmed session override or known configured policy; distinguish no override from direct/unbound routing |
| Activity | Client-observed lifecycle: choosing agent, connecting, ready, working, permission needed, cancelling, or disconnected/failed |
| Session cost | Cumulative native-session `UsageUpdate.cost`, labelled with known provenance |

Activity may include a reported current tool title and client-measured elapsed
time. Without a reported tool, say **Working**. Never invent a percentage,
completion estimate, or a claim that the agent is thinking. Permission-needed
and failure states outrank ordinary activity. Completion may leave a concise
duration/result event while activity returns to ready.

Route display must not equate an agent's model setting, a session lease, and
the actual upstream used by a request. Use **default (no override)** when that
is the only known fact. Show **direct** only when launch/routing state proves
it; otherwise use **unreported**. Route details can expose the agent model and
latest observed upstream separately when those facts are available.

Cost reuses the existing provenance contract: `bitrouter.dev/cost = router`
means router-attributed; an unmarked agent figure is labelled **agent-reported**;
missing or unknown provenance renders **cost unreported**. An inspector may
explain the latter. Never substitute host-wide spend, sum router and agent
figures, convert currencies, or turn missing data into zero. Preserve supplied
pricing/completeness qualifications when available. Cost may arrive late; ready
does not imply that metering has finished. Loaded/resumed session cost remains
session cumulative, not cost since this UI process started.

Context usage is available in session details when the agent reports a usable
used/size pair. It is not a fifth default status item in this proposal.

### 6.2 Terminal sizes

At 80×24, all four fields must be readable, using two status rows if needed.
Long agent/route names may be ellipsized with full text in details; preserve
activity meaning and cost provenance before optional names or hints. At 40×16,
wrap the status and show bounded transcript/composer areas rather than clipping
an approval choice. Below the supported layout minimum, show a resize message
and retain all input/session state. No action is approved by resizing.

Use terminal-aware widths, grapheme-safe editing, plain labels in addition to
colour, and readable light/dark contrast. Keep the terminal's editing cursor
at the actual composer position. The transcript, composer, and focused surface
need distinct visual treatment with minimal borders.

## 7. Composer, commands, and follow-ups

### 7.1 Editing

The composer supports left/right and word navigation, Home/End, deletion,
multiline editing, and Up/Down prompt history at the first/last logical line.
History is process-local for this release. Bracketed paste preserves line
breaks and never submits. A large paste may have a collapsed display, but its
complete contents must be inspectable and sent unchanged.

Enter sends at idle. Shift-Enter inserts a newline where supported; Ctrl-J is
the documented fallback. Ctrl-G opens `$VISUAL` or `$EDITOR` when configured,
using a deliberate terminal suspend/restore path. This handoff is available
only at idle with no pending question in the first release; drafting inside
the TUI remains available during work. On editor failure or session disconnect
preserve the recoverable draft and display the error. The same editor rules
apply in both canonical and retained compatibility entry paths. Blank-input
detection may inspect trimmed text; ordinary prompt submission must preserve
the draft's actual whitespace and line breaks.

### 7.2 Discovery and command ownership

Ctrl-P opens a searchable command palette. Typing `/` at the start of the
composer opens slash completion. Rows contain a label, a short description,
owner (**BitRouter**, **Agent**, or **Prompt template**), and a reason when an
offered action is unavailable. Arrow keys navigate; Enter accepts; Esc closes.
Digits are query text in searchable pickers, never immediate selection keys.

Keep the existing local command spellings and resolution precedence for direct
typed submissions. Do not invent aliases such as `/model` or `/agent` that
silently reserve another agent's commands. View-only actions such as **Open
session** or **Agent settings** can initially be palette entries without new
slash names. Existing `/route` is a session mutation; `/preview` is read-only.

When names collide, show both owners. Selecting an Agent row explicitly sends
that agent command as prompt text, bypassing local name resolution; selecting
a BitRouter row dispatches its typed local action. The selection must retain
owner identity until submission. If edited so that the selected command no
longer matches, clear that identity and resolve again visibly. Prompt templates
remain prompt expansions. Unknown typed slash text retains the existing agent
prompt fallback. No UI command is executed by asking the model to perform it.

Agent command lists update from `AvailableCommandsUpdate`. A list not yet
received is different from an empty list. Shared operational actions continue
through `ACTIONS` and existing ports. Pure UI actions do not become public MCP
tools merely because they appear in the palette.

### 7.3 While a turn is running

Draft editing stays available. Tab explicitly queues the draft for the next
turn when no completion popup owns Tab. With a popup open, Tab accepts its
completion; it must not also queue. Enter during a running turn preserves the
draft and displays **Tab queues for the next turn; Esc interrupts**. It does
not claim Codex-style steering or silently interrupt and resubmit.

Queued prompts appear above the composer and can be inspected, edited, or
removed through a focused queue action. FIFO dispatch sends one item only after
the preceding `session/prompt` has completed normally with `end_turn` and all
blocking requests are settled. A refusal, limit stop, error, cancellation, or
disconnect pauses the queue for explicit user action. Never replay uncertain
submissions after reconnect or move queued work to a different agent/session.

The first queue supports text prompts, prompt templates, and advertised agent
commands. Revalidate command availability when dispatching. Local UI/settings
actions execute through their own availability rules; do not add them or shell
commands to this queue. New-session/agent switching with queued work requires
the user to resolve or discard that queue explicitly.

### 7.4 Keys and modal precedence

| Context | Esc | Ctrl-C | Ctrl-D |
| --- | --- | --- | --- |
| Picker/inspector | Close and restore | Close and restore | No session exit through a modal |
| Working composer | Request cancellation | Request cancellation | No action; show exit guidance |
| Cancelling | Leave cancellation running | Leave cancellation running | No action |
| Ready composer with draft | Clear completion/selection only | Clear draft | Preserve draft; explain empty-draft exit |
| Ready empty composer | No action | Exit | Exit |
| Focused permission | Dismiss using the explicit reject/cancel rule in §9 | Cancel the turn | No approval or session exit |

Process INT/TERM/HUP and input closure still use the shared teardown path.
Ctrl-L redraws without clearing the conversation or draft. Hints are contextual;
no key both selects an option and types into the draft, or both closes a modal
and exits. Tab never cycles global pages.

## 8. Transcript and inspection

Keep the retained journal as the authoritative in-memory projection of ACP
events. Presentation groups may combine adjacent completed reads/searches but
must retain their underlying IDs, order, status, and content for inspection.
Do not merge across user/agent messages or hide a failed/pending tool in a
completed group. Unknown tool kinds keep a generic visible representation.

Use restrained Markdown rendering for agent messages: paragraphs, lists,
headings, fenced code, and links. Preserve exact code text when copying. Treat
terminal control sequences in output as content to sanitize, never commands to
the terminal. Do not infer structured plans from arbitrary prose.

Completed routine tools default to a compact summary; failed tools expose a
bounded useful diagnostic immediately. Full tool output and ACP-provided diffs
open in an inspector. A truncation indicator must offer a path to the retained
content; if the upstream itself truncated it, state that the remainder was not
received. Do not fetch or execute a command to reconstruct missing output.

Transcript inspection supports search and copying a selected message/tool
output, with a terminal-appropriate fallback if clipboard integration is
unavailable. File locations and structured diffs use ACP data when supplied;
they must not claim to be a complete working-tree Git diff. Terminal-reference
content remains a labelled reference unless terminal tooling is negotiated and
implemented; this spec does not turn it on.

Scrolling away from the live tail enters **reading history**. Incoming events
update the journal and a new-activity indicator, never reset the scroll anchor.
An explicit **Return to live** action resumes following. Anchor by retained
entry and intra-entry offset so wrapping, resize, and tool expansion do not
arbitrarily change the reading position. A user submission can explicitly
return the view to live. A permission arriving during inspection raises an
attention indicator; it does not silently discard the inspection position.

## 9. Permissions and cancellation

Render the agent's actual option labels and preserve their exact IDs. Show the
tool title, kind, and available structured command/diff/location context; retain
a generic fallback for absent fields. No locally invented **Always allow** or
permission option may be sent to the agent. Do not interpret ToolKind alone as
proof that an operation is safe or enforceable by an OS sandbox.

Permission requests are queued by identity and resolved exactly once. A second
request cannot overwrite the visible request. Show pending count where useful.
Preserve the user's draft throughout the interaction. Newly arrived permission
UI must not consume buffered composer keystrokes as consent: require explicit
focus/selection after presentation, with no preselected approval. Numeric keys
may highlight an option but must not immediately authorize it; Enter confirms
an explicitly selected option. F2 focuses the oldest pending permission, with
the same action available in the palette. Show that shortcut in the activity
hint; it works while reading history or editing a draft and is inert when no
permission is pending.

Dismissal while leaving the turn running chooses an offered reject-once option
when available; it must not silently grant persistent rejection when both
reject-once and reject-always exist. Otherwise use the protocol's cancelled
outcome rather than inventing an option. Turn cancellation is separate: resolve
all pending permissions with `RequestPermissionOutcome::Cancelled`.

On user cancellation:

1. Enter **Cancelling** and prevent another prompt or session replacement.
2. Settle pending permission requests with the cancelled outcome and send
   `session/cancel` once. Newly arriving permissions for that turn also cancel.
3. Continue consuming final tool/message updates and observe the original
   prompt response. Do not drop the future and immediately report ready.
4. Report the actual result. A completion racing cancellation is not rewritten
   as successful cancellation; partial effects are not described as rolled back.
5. If the agent fails to settle within the shared bounded cancellation grace,
   show failure/disconnected state and use controlled teardown. Do not reuse an
   uncertain connection or automatically submit queued work.

Reuse the existing client timeout/grace machinery rather than introduce a
second timer policy in a renderer. Journal tool presentation may show a local
cancellation annotation while preserving the actual protocol status. This is
not a new wire enum. Terminal restoration and permission cleanup must also hold
on input closure, shutdown signals, adapter failure, and UI error.

## 10. ACP-led settings, sessions, and routing

Build **Agent settings** from initial session configuration/mode data and later
updates. Apply supported changes through the appropriate standard session
method; update displayed values only from confirmed results. Use configuration
categories for placement, not correctness. Missing/unknown categories fall
back to the agent's labels/order; unknown option types are not actionable.
No native-agent name grants a capability. The first UI offers settings changes
at idle even where an agent could accept them during generation.

At the source baseline, the client has new/load/resume methods and a wider
capability snapshot. A snapshot flag alone is not an implemented client operation. Native
session listing/config setters needed by a picker require typed client support
and protocol tests before exposure. Do not add fork/delete or enable unrelated
unstable schema features to complete this UI. A load-only agent gets native-ID
entry; a list-capable agent gets pagination/search over its native results.

Load replays native history. Resume continues without replay and must say
**Earlier history was not replayed** rather than draw an apparently fresh
conversation. Subscribe/buffer updates before opening so replay is not lost.
Seed the view from initial session metadata as well as streamed changes. IDs
and transcript storage remain harness-owned; the renderer stores only the
current process's projection and drafts.

Session routing uses the existing `_bitrouter/route/*` extension only when its
version, session scope, and required methods are advertised. Route selection
and reset are idle-only; confirmed failures leave the old route displayed. Reset
means drop this session's override, not reset agent settings or daemon config.

The routable-model catalog and route preview use the existing shared report
ports. Preview is declared/configured resolution, not a promise to reproduce
all prompt-dependent routing, capability filtering, hooks, or fallbacks. A
request inspector labels actual observations separately. If only host-wide
request history is available, label it **Host requests**; do not infer a session
filter from timing or model names. Session-specific inspection requires proven
identity correlation in the underlying report.

## 11. Implementation boundaries

| Owner | Responsibility |
| --- | --- |
| `crates/bitrouter-tui` | Composer, focus/modal state, permission/queue data, retained journal, presentation, scrolling, and pure interaction transitions |
| `apps/bitrouter/src/chat` and canonical Code driver | Input and async event orchestration; render effects, never own daemon configuration/storage |
| `apps/bitrouter/src/actions` | Injected typed operational ports and reports for local/remote scope |
| `apps/bitrouter/src/acp_cli.rs` | Agent resolution, preparation, authentication boundary, session host, lifecycle capability snapshot |
| `crates/bitrouter-sdk/src/acp/client.rs` | Typed ACP methods, permission outcomes, cancellation, and protocol lifecycle |

Use one interaction core and one permission/turn lifecycle for every
BitRouter-owned interactive conversation path. Retained hidden compatibility
entries must delegate to it; no independent dashboard-versus-inline command or
approval semantics remain. A presentation wrapper may differ for terminal/pipe
output. Keep the existing noninteractive output contract and protocol-pure ACP
stdout intact. Pure UI queueing and focus do not leak into headless execution.

Preserve the dependency direction: the TUI crate cannot access app config,
HTTP, IPC, or metering, and the app does not acquire a ratatui dependency.
Reuse the existing searchable selection state where practical, without keeping
duplicate digit-selection semantics. Remove the seven-page enum, page-cycling
handlers, and unused renderers once their capabilities have replacements.

All long effects (connection, reports, settings, session opening) must leave
input, cancellation, and ACP updates responsive. Bound render work through a
schedule and dirty state; do not redraw an entire retained history for every
stream chunk or keep an idle animation timer alive. Optimize from measured
terminal behavior, not an invented frame-rate target.

## 12. Delivery sequence and acceptance

These are implementation slices of the authorized release.

1. **Unify behavior:** consolidate interaction and permission lifecycle, add
   cancellation completion handling, and preserve canonical/headless boundaries.
2. **Replace navigation:** conversation entry, command palette, four-field
   status, transient inspectors, and local/remote operational parity. Remove
   permanent tabs and page cycling.
3. **Complete composition:** multiline editing, preserved paste, cursor/history,
   external editor, and explicit next-turn queue.
4. **Complete inspection:** stable history reading, searchable full output,
   structured diff inspection, settings and supported session pickers.
5. **Validate and document:** real-terminal journeys, minimal-capability agents,
   supported adapter smoke tests, and source-of-truth documentation updates.

| ID | Required observable result |
| --- | --- |
| A1 | Bare local `code` offers agent selection within an empty conversation; explicit agent invocation reaches conversation; neither draws seven tabs |
| A2 | Every dismissed picker/inspector restores draft, cursor, focus, and reading anchor; active ACP updates are still consumed |
| A3 | Agent, confirmed route, activity, and attributed session cost remain readable at 80×24 and 40×16; unknown cost never becomes zero |
| A4 | Cursor editing, multiline Unicode/CJK/grapheme input, paste, history, and external editor preserve intended prompt bytes and terminal state |
| A5 | Drafting during a turn loses no input; Tab queues explicitly; Enter never claims unsupported steering; queued work runs serially and pauses on abnormal stops |
| A6 | Reads during streaming stay anchored; return-to-live is explicit; resize and expanded output preserve location |
| A7 | Two overlapping permissions retain identities and labels; only explicit post-presentation selection authorizes; dismissal, turn cancellation, and teardown have distinct correct outcomes |
| A8 | Cancellation accepts late updates, observes settlement or bounded failure, prevents overlapping prompts, and never replays uncertain work |
| A9 | Local/agent command collisions are visible and both owners are reachable; legacy typed precedence and prompt-template behavior remain deterministic |
| A10 | A minimal ACP agent with no commands, usage, route control, or optional lifecycle/settings still supports an ordinary conversation and honest unavailable states |
| A11 | Agent settings and route changes display confirmed results; failed mutations retain previous values; load/resume semantics and native IDs remain distinct |
| A12 | Remote code retains supported read-only operations without agent execution, route mutation, or local-data fallback; scope is visible |
| A13 | Tool summaries preserve full received output and structured diffs; unknown kinds remain inspectable; no agent text is mistaken for BitRouter evidence |
| A14 | Canonical and retained compatibility paths share command/permission behavior; stdout contracts and terminal restoration survive exit, editor handoff, signals, and adapter failure |

Use reducer and renderer tests for transitions/layout, protocol stubs for ACP
ordering/capability behavior, and real-PTY journeys for the integrated loop.
Fixtures must cover normal end, refusal/limit stop, delayed cancellation,
duplicate/overlapping permissions, load replay, missing usage, unknown metadata,
and absent optional capabilities. Use bounded condition waits and deterministic
semantic assertions; arbitrary sleeps and AI-approved screenshot changes are
not acceptance oracles. Tests use isolated directories and no live credentials.

Real-PTY checks include `stty` restoration and a usable shell after exit. A
visual review checks compact/wide layouts and long streaming output, but does
not replace protocol/behavior assertions. Native adapter smoke checks record
adapter versions and actually advertised capabilities; do not derive an
adapter feature matrix from the native CLI documentation alone.

Before source submission run the required all-features tests, clippy, and fmt
checks from [`AGENTS.md`](../AGENTS.md), plus relevant ACP/CLI output and
dependency-boundary guards. The original documentation proposal needed link and diff validation;
the implementation must pass the full source gates above.

## 13. Documentation and compatibility work when implemented

Update [`CLI.md`](CLI.md), [`DEVELOPMENT.md`](DEVELOPMENT.md), affected design
spec status notes, and [`skills/bitrouter/`](../skills/bitrouter/SKILL.md) in
lockstep with the changed entry behavior, keys, and session wiring. Keep the
shippable skill under its documented size limit and use references for detail.
Check `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json`
only for affected invocation changes; this spec does not rename `mcp serve`.

Public naming (`code`, `run`, native launchers, `acp serve`) stays as-is. Retain
existing hidden CLI aliases without proliferating more. Document the intentional
interactive key changes and removal of page navigation. Product-site prose is
owned by `bitrouter-docs`; do not put contributor-only spec material in the
shippable skill or hand-maintain generated catalog tables.

## 14. Research basis

Official documentation checked on 2026-09-08. These sources motivate the
interaction design; they do not promise the same functionality through ACP.

- [Codex CLI commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli):
  model/session pickers, slash discovery, configurable status fields, and
  distinct queued versus steering input. Adopt the focused interaction pattern;
  do not copy native steering semantics into a generic ACP prompt loop.
- [Claude Code commands](https://code.claude.com/docs/en/commands) and
  [interactive mode](https://code.claude.com/docs/en/interactive-mode): controls
  are invoked within a session; transcript inspection preserves editing state.
- [Claude Code fullscreen rendering](https://code.claude.com/docs/en/fullscreen):
  alternate-screen rendering is compatible with a conversation-led product.
- [OpenCode TUI](https://opencode.ai/docs/tui/) and
  [keybindings](https://opencode.ai/docs/keybinds/): command discovery, transient
  selection, detail inspection, and optional sidebar access. The optional
  sidebar is not a requirement for BitRouter's first version.
- [ACP prompt lifecycle](https://agentclientprotocol.com/protocol/v1/prompt-turn)
  and [tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls):
  update ordering, permission choices, cancellation outcomes, and structured
  tool evidence.
- [ACP configuration options](https://agentclientprotocol.com/protocol/v1/session-config-options),
  [slash commands](https://agentclientprotocol.com/protocol/v1/slash-commands),
  and [session setup](https://agentclientprotocol.com/protocol/v1/session-setup):
  dynamic client controls and native lifecycle boundaries.

The recommendation is a design inference from these patterns and the source
audit. This proposal intentionally changes information hierarchy before
attempting to match every visual or native feature of another coding agent.
