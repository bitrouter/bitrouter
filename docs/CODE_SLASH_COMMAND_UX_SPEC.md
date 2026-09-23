# Code slash-command and hotkey UX

**Status:** implemented locally; all-features tests, Clippy, and formatting passed.

## 1. Scope and existing behavior

This spec changes command discovery and invocation in `bro code`, including its
background-agent deck and operations-only remote view. It supersedes the
`Ctrl-P`/leading-slash split and the default action bindings in
[`CODE_TUI_UX_SPEC.md`](CODE_TUI_UX_SPEC.md) and
[`BACKGROUND_AGENT_UX_SPEC.md`](BACKGROUND_AGENT_UX_SPEC.md). Their terminal
ownership, native scrollback, dock limits, session ownership, permissions,
background-run leases, and confirmation rules remain the governing design.

Before this change, the bottom rail advertised `F2` permission, `F3` queue, `F4` detail,
`F5` agents, and `Ctrl-P` commands. Some actions exist only in the full palette.
Typing a leading slash opened a smaller palette, but its query was copied into
the composer draft. This makes the command path depend on terminal F-key
delivery, memory of context-sensitive bindings, and a draft that can be changed
while browsing commands.

## 2. Product contract

1. `/` opens one searchable, flat command launcher. Every interactive Code
   action has a visible row, searchable by its plain-language name. Frequent
   actions may also have a short direct slash name, such as `/new`; users need
   not memorize any name to reach an action. The inventory contains BitRouter
   actions, active-agent commands, and prompt templates, marked by owner. It
   is not a mirror of every headless `bro` CLI leaf.
2. There are **no default action hotkeys**. Users may bind actions in their
   personal Code keymap. `/hotkeys` displays their effective bindings and the
   commands that are unbound.
3. Opening `/` does not write `/` or the search query into an existing prompt.
   `Esc` closes the command input and restores the exact draft and focus.
4. Command execution never silently replaces, submits, or queues a prompt
   draft. Mutating actions retain their existing authorization and confirmation
   boundaries.
5. The permanent rail advertises current state and a short `/ Commands` cue;
   it does not try to teach a changing set of F keys. A permission notice uses
   plain language such as `Permission needed · / to review`.

“No default action hotkeys” excludes command-specific bindings such as F2–F5,
`Ctrl-P`, `Ctrl-O`, `Ctrl-G`, `Ctrl-Y`, `Tab` to queue work, and the deck's
single-key verbs. Ordinary input controls still work: text editing, arrows and paging, `Enter` to submit
the focused input or activate an explicitly selected row, `Esc` to back out,
and terminal interruption/exit controls. Their exact per-surface behavior must
be documented; they are not entries in the configurable action keymap.

## 3. Opening and closing the command input

### Trigger and literal slash

- From a focused composer, reply field, or new-run prompt, `/` at **offset 0**
  opens the temporary command input, even when that field already contains a
  draft. Move to the start of a draft, then type `/` to reach commands. At any
  later offset, `/` inserts a literal slash. This keeps paths and ordinary
  prose easy to type.
- From a non-text surface that is safe to leave temporarily (conversation,
  agent list, queue, inspector, status view), `/` opens the same input. The
  agent list's current `/` filter moves to an explicit search affordance; it
  must not intercept the command trigger.
- A literal slash at offset 0 is entered by typing `//`: the second slash
  dismisses the still-empty command input and inserts one slash at the
  original cursor. Pasting a leading slash into a prompt remains data and
  never invokes a command.
- A permission choice or destructive confirmation retains focus. `/` may open
  the command input for navigation, but the focused request/confirmation is
  never approved by doing so. Returning to it resets any selected approval
  option; a fresh explicit choice and `Enter` are required.

The command input is an overlay with its own query, selection, and return
target. It is not an `Editor` draft. The focused editor remains untouched, so
its text, cursor, history-navigation state, and target identity survive cancel.
The return surface retains its relevant selection and scroll state. This is
in-memory state: no prompt is added to history and no native terminal
scrollback offset is promised.

`Esc` restores that snapshot exactly and consumes the key. In particular it
must not also deny a permission, interrupt a turn, detach a run, or leave Code.
Closing a nested picker returns to the command input first; a further `Esc`
returns to the original surface. A command that intentionally navigates to a
new surface consumes the overlay but preserves the source draft until the user
explicitly sends, edits, replaces, or discards it. If the underlying target
run/session disappears while the overlay is open, closing shows a clear
unavailable state and keeps the draft under its original identity; it must not
reattach the text to another run.

### Search and activation

The input accepts a direct name or natural-language search over names, aliases,
descriptions, and owners. Results show `BitRouter`, `Agent`, or `Template`,
their current availability, and why a disabled action is unavailable. Search
and selection use stable action identities, not row indices, when asynchronous
state changes. An unavailable result cannot be activated. `Enter` invokes
only the currently selected, available result. Empty search opens the command
inventory with no destructive action preselected. Pasted text, including a
newline, changes the query only; it cannot activate a row.

With an empty query, show actions relevant to the current state first (for
example **Review permission** when one is pending), followed by the rest of
the inventory. Search always reaches every valid action; context changes
ranking and availability, not the meaning of a name. Destructive rows carry
explicit verbs and are never the default `Enter` target.

Invoking a read-only or navigational command preserves the draft. Agent
commands are sent as their own explicitly selected command; they do not
replace or submit the saved human prompt. A prompt template asks for an
explicit **Replace draft** choice; `Esc` keeps the draft. While a foreground turn is active,
agent commands follow the existing queue policy and show the proposed queued
item before the user commits it. Command input acceptance alone cannot enqueue
it. Background drafts remain bound to their run while the launcher is open;
agent commands and templates are offered when the foreground composer has focus.

## 4. One command inventory

The launcher replaces the split between `Ctrl-P`-only actions and the smaller
leading-slash palette. It uses the existing typed-action and ACP command
sources rather than a separate list of handlers. The visible rows are flat:
searching `reload` shows both **View reload state** and **Reload now**; selecting
the latter still requires its existing authorization and confirmation. No
new `/topic subcommand` grammar is needed for navigation.

| User task | Launcher row | Direct name |
| --- | --- | --- |
| Browse commands and help | Help | Existing `/help` and `/commands` remain. |
| Review pending foreground permission | Review permission | None required; focus oldest pending request with no option preselected. |
| Inspect or resume queued follow-up | Review queue; Resume queue | None required; show availability and reason. |
| Inspect selected journal item | Inspect detail | None required; disable when absent. |
| Open background-agent deck | Background agents | None required. |
| Choose active agent or open native session | Choose agent; Open session | None required. |
| Start fresh native session | New session | `/new`; preserve current agent launch settings. |
| Show session identity and usage | Session details | None required. |
| Open agent settings | Agent settings | None required. |
| View effective keymap | Hotkeys | `/hotkeys`. |
| Existing typed actions | Status; Routable models; Route preview; Session route; Reset session route | Preserve `/status`, `/models`, `/preview`, `/route`, and `/route reset` for compatibility. |
| Inspect host requests, providers, or telemetry | Host requests; Providers; Telemetry | None required. |
| Inspect policy or agent catalog | Policy status; Policy detail; Agent catalog | None required. |
| Inspect or trigger reload | View reload state; Reload now | None required; `Reload now` retains confirmation and scope checks. |
| Open editor, transcript, or search | Open external editor; Open transcript; Search detail | None required; retain contextual availability. |
| Copy foreground detail or export a background run | Copy detail; Export run detail | None required; require supported detail or run. |
| Detach session and exit | Detach and exit | None required; explicit supervised-run exit action. |

The remote operations-only view exposes only rows its target supports. The
launcher does not expand its ACP execution surface. The implementation
inventory must account for every current palette item and contextual action
before removing its default shortcut; a missing row is a migration defect.
Existing `/evolution` and live ACP commands keep their owners and availability.
Existing multiword typed forms, such as `/route reset`, remain compatible but
are no longer the model for new Code interactions.

The same search term or direct name can identify BitRouter, agent, and
template rows. The result list shows all owners. If a direct name matches more
than one owner, `Enter` opens an owner choice; it does not silently run the
local command. `/new`, `/hotkeys`, and `/help` remain reachable as BitRouter
rows even if an agent advertises the same text. The chosen agent command is
sent with its original spelling. A command's owner, availability, and side
effect must be visible before activation.

The background-agent deck contributes flat, contextual launcher rows: **Reply
to run**, **New background run**, **Attach run**, **Peek run**, **Review run
permission**, **Take over run**, **Cancel run**, **Stop run**, **Mark run
reviewed**, and **Detach from run**. Each shows its target identity, retains
the existing lease checks and confirmation steps, and is unavailable when its
target is missing or unsuitable.
Arrow keys, paging, and `Enter` on a clearly labeled selected row remain
navigation. The single-key verb keys, including `Space` for peek, cease to be
implicit command shortcuts.

## 5. Configurable hotkeys and `/hotkeys`

The keymap is per user and applies to Code surfaces, not router policy or a
shared project configuration. It binds key chords to stable action IDs in a
JSON object at `$XDG_CONFIG_HOME/bitrouter/code-hotkeys.json`, or
`~/.config/bitrouter/code-hotkeys.json` when XDG config home is unset. For
example, `{"F2":"review_permission","Ctrl-P":"hotkeys"}`. There are no
shipped action bindings or per-surface scopes.

Validation rejects duplicate chords, unrecognized action IDs, and chords that
conflict with required text editing or safety controls. Invalid keymaps leave
all action bindings disabled and show a diagnostic. A terminal that intercepts
a requested chord before delivering it to Code cannot be detected by Code;
users can select the same action with `/`.
User bindings never bypass availability, target identity, or confirmations.
They resolve to the same action as selecting its launcher row.

`/hotkeys` shows bound chords and action IDs, unavailable bindings, and unbound
commands. It shows the config path and any validation diagnostic. The compact
rail retains `/` as the stable cue.

## 6. Permission, background, and narrow-terminal rules

- Permission arrival remains a notice and does not take input focus. Choosing
  **Review permission** from `/` is an explicit focus transfer; clear any
  approval selection.
  A later buffered `Enter`, digit, or arrow from the command input cannot
  authorize the request. Only a fresh selection followed by `Enter` can do so.
- When a background agent needs input, **Background agents** opens the deck,
  and **Review run permission** opens that run's review surface. Foreground
  permission priority and read-only background peeks remain in force. Run ID,
  permission ID, request ID, and lease generation must still match at commit.
- Reply and new-run drafts remain bound to their original run/target through
  overlay navigation, detach, and return. If a selected run changes while the
  launcher is open, run-target actions become unavailable; the draft stays
  under its original identity.
- All command rows have a short text label; color or a key glyph alone is not
  enough. At 40×16, the input and permission context must remain legible. Below
  the minimum size, permissions remain impossible to approve and the UI gives
  resize guidance without relying on an F key.
- Attached terminals and external editors own their own keystrokes. Code
  cannot treat their `/` input as a command trigger. On return to Code, `/`
  works on the focused Code surface again.

## 7. Acceptance and implementation boundaries

| Case | Required result |
| --- | --- |
| Nonempty multiline draft with Unicode text and history recall active; move to offset 0, open `/`, then `Esc` | Exact draft, opening cursor, history-navigation state, and target restored; no history or queue mutation. |
| Type `/` inside text, type `//` at offset 0, paste `/foo` | Literal slash/data in the draft; no command execution. |
| Open `/` from agent list, queue, inspector, and operations-only status; press `Esc` | Return to the same surface and relevant selection/scroll position. |
| Permission arrives while command input is open | Notice only; `Enter` in the command input cannot approve it. **Review permission** enters review with no choice selected. |
| Agent command collides with a BitRouter row or direct name | Both owners visible; a direct ambiguous entry asks for owner before execution. |
| Active turn and nonempty draft; select an agent command or template | Draft remains intact; queuing or replacement requires a separate explicit choice. |
| Background reply draft and run changes while input is open | No retargeting or stale permission answer; text remains recoverable under original identity. |
| Empty/default keymap, then a user binding | No F-key/action shortcut works by default; configured chord invokes the same guarded action, and `/hotkeys` displays it. |
| Narrow terminal and paste with newline | No hidden approval/activation; readable resize guidance when permission choices do not fit. |

Implementation updates the Code input/state model, background deck, rail and
help copy, focused interaction tests, [`CLI.md`](CLI.md), and the shipped
BitRouter skill. Product documentation belongs in `bitrouter-docs`.

## 8. Adopted decisions

1. **Slash trigger:** `/` at editor offset 0 opens commands; `//` enters a
   leading literal slash. A slash elsewhere in a draft is text.
2. **Direct names:** `/new` and `/hotkeys` are built in. Existing typed forms
   stay compatible, while other actions are reached through flat rows and
   search without new multiword slash commands.
3. **Keymap storage:** The optional per-user JSON object uses chord keys and
   stable action ID values at the XDG path above.
