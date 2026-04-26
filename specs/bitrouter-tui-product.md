# BitRouter TUI — Product Documentation

> Draft v4 (target design). Reflects the redesign decisions:
> 1. No broadcast (`@all` removed) — explicit routing only.
> 2. Input bar fixed at the bottom; thin status bar below it.
> 3. **Session = thread.** One live agent process bound to one
>    conversation. Multiple sessions can coexist for the same or
>    different agents. Top-bar tabs show active sessions.
> 4. **No modals.** Every former modal flow (help, observability,
>    command palette, import) becomes a slash command that renders
>    into scrollback.
> 5. **Keybindings are minimal.** If a behavior can be done via slash
>    command + inline picker, it has no keyboard shortcut — with two
>    intentional exceptions: `Tab` / `Shift+Tab` cycle the active
>    session (because tab-cycling is a tab UI convention users
>    already have muscle memory for), and `?` invokes `/help`
>    (because help has to be one keystroke from a blank prompt).
> 6. **Live slash autocomplete.** Typing `/` opens a filter-as-you-type
>    command popup, just like `@`-mentions.
> 7. **Subtle activity indicator** on the status bar's agent/model
>    slot when a turn is in flight.
>
> Source of truth for the *current* implementation is the code in
> `bitrouter-tui/src/`; this doc is the target the implementation will
> converge on. Sections marked *(NYI)* differ from today's code.

## 1. Overview

`bitrouter-tui` is the terminal front-end for BitRouter. It manages
**coding-agent sessions** in real time — each session is a live
JSON-RPC conversation with an Agent Client Protocol (ACP) compatible
CLI agent (Claude Code, Codex, Gemini, OpenCode, Copilot, etc.).

The TUI lets a user:

- Discover, install, and connect ACP agents on their machine.
- Run **multiple concurrent sessions** in a single window, including
  more than one against the same agent, accessible via top-bar tabs.
- Manage every session operation (new, switch, close, list, import,
  rename) via the `/session` slash command — there is no sidebar or
  modal session manager.
- Address a specific agent inline with `@-mentions` (no broadcast).
- Approve/deny tool-call permission requests inline,
  mid-conversation.
- Watch streaming responses, tool calls, and "thinking" output as
  they arrive, with markdown rendering and per-agent color coding.

The TUI is launched automatically when running `bitrouter` (the `tui`
crate feature is on by default). The HTTP proxy that BitRouter exposes
to LLMs continues to run in the same process; the status bar shows the
active agent and resolved model.

## 2. Core concepts

### 2.1 Agent and Session

Two objects:

- **Agent** — an ACP-capable binary on the user's machine (e.g.
  `claude-agent-acp`). One install per agent id; shared by every
  session that connects to it.
- **Session** — one live ACP subprocess **plus** the conversation it
  is having, exposed as a tab in the top bar. The unit of
  multi-tasking. A session is created by spawning a fresh agent
  process or by loading an on-disk conversation; it ends when the
  user closes its tab.

> **One session is one thread.** "Session" and "thread" describe the
> same object — a session is the live thread; a thread is a session
> with a process attached. We use **session** throughout this doc to
> match the ACP protocol and the existing code.

Multiple sessions can coexist for the same agent (two Claude Code
sessions exploring different parts of the codebase) or across
different agents (Claude Code + Codex side-by-side).

### 2.2 Session sources

Every session has a source:

- **Native** — created fresh in this TUI process (`/session new`).
  Starts empty; gets its title from the first user prompt.
- **Imported** — loaded from an agent's on-disk history via ACP
  `session/load` (`/session import`). Title comes from the agent's
  metadata when available. The replay ends with a horizontal
  `── imported history ──` divider in the scrollback; everything
  below the divider is new.

Closing a session ends the agent subprocess and removes the tab. The
conversation may still exist in the agent's on-disk storage and can
be re-imported later.

### 2.3 Project scope (cwd)

The TUI assumes a single project per process: the **launch cwd** is
the working directory the user ran `bitrouter` from. All new sessions
are spawned with this cwd as their root, and the on-disk session scan
only looks at conversations that match this cwd. There is no in-app
project picker — to switch projects, restart the TUI in a different
directory.

### 2.4 Targets and `@-mentions`

The input bar routes prompts to **targets**:

- **Default** — no `@-mention`. Routes to the **active session's**
  agent.
- **Specific** — `@claude-code` (or any agent name) routes to that
  agent. If no session for that agent exists, one is created on the
  fly. Multiple `@-mentions` in one prompt fan out — each agent
  receives its own copy in its own session.

Mentions are case-insensitive, deduplicated, and stripped from the
prompt body before it goes over the wire. `Tab` completes the
`@-prefix` against the agent list while typing.

> **No broadcast.** `@all` is intentionally not supported. Sending the
> same prompt to every agent on the machine wastes tokens and is
> rarely the user's actual intent. To address several agents at once,
> name them explicitly: `@claude-code @codex …`.

## 3. Window layout

```
╭─ ● claude-code  ◎ codex  ◌ gemini  +───────────────────────────╮  ← top bar: session tabs
│                                                                 │
│  ... scrollback (full width, no sidebar) ...                    │
│                                                                 │
│   › refactor the router                                         │
│                                                                 │
│   claude-code                                                   │
│   ⎿  Sure — let's start by reading the current routing module.  │
│   ⎿  ⠋ read_file(src/router.rs)                                 │
│   ⎿  …                                                          │
│                                                                 │
│  ~/Documents/Code/bitrouter                                     │  ← cwd label
│  ───────────────────────────────────────────────────────────────│  ← input divider
│  › ▌                                                            │  ← input bar (fixed)
├─────────────────────────────────────────────────────────────────┤
│  /  commands  ·  ?  help                  claude-code · sonnet  │  ← status bar (1 row)
╰─────────────────────────────────────────────────────────────────╯
```

The TUI has exactly **four regions**, top to bottom:

1. **Top bar (1 row)** — session tabs.
2. **Scrollback (fills available height)** — message history.
3. **Input bar (1+ rows, fixed at bottom)** — multi-line prompt
   input.
4. **Status bar (1 row, fixed at bottom)** — slash/help affordance
   left, active agent + model right.

There is **no sidebar**. There are **no modal overlays**.

### 3.1 Top bar — session tabs *(NYI)*

The top bar lists every active **session** as a horizontal tab strip.
Each tab shows:

- A status dot (color-coded by the session's underlying agent
  reachability — see §7).
- The agent id (`claude-code`) or, when set, the user-renamed
  session label.
- A trailing badge: `[N]` for unread activity in a background
  session, `⚠` when a permission request is pending in that
  session.
- A trailing `+` button to spawn a new session (equivalent to
  `/session new`).

The active tab is bold-underlined. Switch tabs by:

- **`Tab`** / **`Shift+Tab`** — cycle to the next / previous tab in
  left-to-right order (wraps at the ends). Scoped to **active
  sessions only** — tabs are the only thing `Tab` cycles, so
  importable-but-not-loaded sessions are never in the cycle.
- **Mouse** — click the tab.
- **`/session switch [<id>]`** — slash command, with an inline picker
  when called without an argument.

There is no `Ctrl+1..9` shortcut and no `Ctrl+Tab` chord — plain
`Tab` is enough.

> When the `@`-mention or slash-command autocomplete popup is open,
> `Tab` accepts the highlighted candidate instead of cycling tabs.
> The popup wins because it's a transient, in-input affordance.

### 3.2 Scrollback

Entries (top → bottom, oldest → newest):

| Kind | Visual |
|---|---|
| User prompt | `› <text>` cyan prefix |
| Agent response | `  <agent-name>` header + `⎿  <markdown>` body in agent color |
| Tool call | `⎿  <icon> <title>` — `○` pending, `⠋` in-progress, `✓` done, `✗` failed |
| Thinking | `⎿  ◌ Thinking…` block, dimmed grey, collapsible |
| Permission | `⎿  ⚠ <title>` + inline prompt `(y)es / (n)o / (a)lways` |
| System | dim grey single line — install progress, errors, slash-command output, inline pickers |
| Separator | horizontal divider with optional centered label |

Markdown in agent responses is rendered with code fences, headings,
emphasis, and lists. Per-agent color coding is used in the gutter and
header so multi-agent conversations remain readable.

A live cursor (`▌`) marks the current streaming entry. Tool calls
auto-collapse when they complete or fail; thinking blocks
auto-collapse when the turn ends.

### 3.3 Cwd label and input bar

Just above the input bar, a single dim line shows the current working
directory (e.g. `~/Documents/Code/bitrouter`) — it's a quiet reminder
that everything you type is rooted here.

A horizontal rule separates the cwd line from the input. The input
bar itself is **always fixed at the bottom** of the scrollback area
and grows upward as the user types multiple lines; the scrollback
above it shrinks to compensate. Multi-line input is supported
(`Shift/Alt/Ctrl + Enter` inserts a line; `Enter` alone submits).

A small `@mention` autocomplete popup floats above the prompt while
the user is typing an `@`-token, listing matching agent names.

### 3.4 Status bar (bottom)

A single, low-density row at the very bottom. **Two elements only:**

- **Left:** `/ commands · ? help` — discoverability hints. Typing `/`
  in the input begins a slash command (see §5); typing `?` followed
  by Enter (or `/help`) prints the inline help.
- **Right:** `<agent> · <model>` for the **active session**. Example:
  `claude-code · sonnet-4.6`. When the session is mid-handshake the
  agent slot reads `connecting…`; on error it reads `error`. When a
  turn is in flight a subtle leading **activity indicator** is shown
  — a slow-rotating spinner glyph (`⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏`) prefixed
  to the agent slot, e.g. `⠹ claude-code · sonnet-4.6`. The glyph
  disappears the moment the agent emits `TurnDone`.

No mode label, no hint clutter, no listen-address. Mode is implied
by where focus is (typing in the input bar = Normal; everything else
is a transient state that's clear from on-screen cues).

## 4. Input modes

The TUI is intentionally near-modeless. Three modes total:

| Mode | Enter via | What it does |
|---|---|---|
| **Normal** | default | Type a prompt or slash command; Enter sends. |
| **Scroll** | `Esc` from Normal | Browse scrollback (`j/k`, `G`); fold entries (`c`); search (`/`); back to input (`i`/`G`). |
| **Permission** | auto, on incoming permission request | `y/n/a` to allow / deny / always. Steals focus, auto-switches sessions on multi-pending requests. |

Everything else (managing sessions, agents, observability, help)
runs through **slash commands** (§5).

## 5. Slash commands

Lines beginning with `/` in Normal mode are parsed as **slash
commands**. Output is rendered into the active session's scrollback as
system messages. Unrecognised `/...` lines surface a `(no such
command)` hint instead of being sent verbatim to the agent.

### 5.1 Live slash autocomplete

When the user types `/` as the first character of the input bar, a
small filter-as-you-type popup appears just above the input —
analogous to the `@`-mention popup, but listing slash commands
instead of agent names.

- The popup shows commands whose name starts with what's typed: `/s`
  filters to `/session …`; `/se` keeps `/session`; `/agen`
  filters to `/agents …`. The match is a prefix match against the
  literal command (not its arguments).
- Each row shows the command + a one-line description (`/session
  new   spawn a new session`).
- `↑` / `↓` move the highlight; `Tab` or `Enter` accepts the
  highlighted command into the input (preserving any args the user
  has already typed). `Esc` dismisses the popup but keeps the typed
  text.
- Submitting (`Enter` again) runs the command.

This replaces the v1 command palette: discoverability lives in the
same input bar the user is already focused on, with the same
filter-as-you-type ergonomics as `@`-mentions.

### 5.2 The inline-picker pattern

Several commands need the user to choose from a list (pick an agent,
pick a session to switch to, pick on-disk sessions to import). The
TUI handles this **without a modal**:

1. The command renders the list as a system entry into the
   scrollback.
2. The TUI enters a **picker micro-mode**: `j/k` move the cursor,
   `Enter` confirms, `Esc` cancels. The cursor is shown by `▸`.
3. On confirm, the entry collapses to a single `✓ <choice>` line in
   the scrollback. On cancel it collapses to `✗ cancelled`.
4. While the picker is open, normal input is suspended; the input
   bar shows a hint like `↑↓ select · Enter confirm · Esc cancel`.

This is the only "modal-feeling" interaction in the TUI, and it is
spatially co-located with the command that produced it.

### 5.3 Sessions

| Command | Effect |
|---|---|
| `/session` (or `/session list`) | Print all active sessions and any importable on-disk sessions for the cwd, grouped by agent. Each row shows `<id>  <agent>  <last-active>  <title>`. |
| `/session new [<agent>]` | Spawn a new session. With no argument, opens an inline agent picker. |
| `/session switch [<id>]` | Switch active session by id. With no argument, opens an inline picker over active sessions. |
| `/session close [<id>]` | Close active or named session (ends the agent subprocess). |
| `/session rename <title>` | Rename the active session's tab. |
| `/session import` | List on-disk sessions; opens an inline picker for multi-select import. |
| `/session import <agent> <id>` | Import a specific on-disk session by agent + external id. |
| `/session prev` / `/session next` | MRU navigation across the focus history. |
| `/session clear` | Clear the active session's scrollback (does **not** end the session). |

### 5.4 Agents

| Command | Effect |
|---|---|
| `/agents` (or `/agents list [--refresh]`) | List registry agents with install status. |
| `/agents install <id>` | Download and install an agent binary; live progress. |
| `/agents uninstall <id>` | Remove an installed agent. |
| `/agents update [<id>]` | Reinstall one (or all) installed agents. |
| `/agents discover` | Re-scan PATH for agents. |
| `/agents disconnect <id>` | Disconnect every active session for that agent. |

### 5.5 Providers

| Command | Effect |
|---|---|
| `/providers` (or `/providers list`) | Show configured LLM providers and credential status. |
| `/providers use <default\|byok>` | Hint to exit the TUI and run `bitrouter init`. |

### 5.6 Diagnostics

| Command | Effect |
|---|---|
| `/help` (or `?` then Enter) | Inline keyboard / command reference. |
| `/obs` | Inline observability summary. Last 50 events plus the agent table. |

### 5.7 Auth (CLI-deferred)

`/login`, `/logout`, `/whoami`, `/usage`, `/keys`, `/init` — each
prints a single hint to exit the TUI and run the corresponding
`bitrouter` CLI command. These flows are CLI-only for now.

## 6. Keyboard shortcuts

The set is intentionally minimal. **The rule:** if a behavior can be
done via a slash command + inline picker, it does **not** get a
keyboard shortcut — with two intentional exceptions:

- **`Tab` / `Shift+Tab`** cycle the active session (forward /
  backward across the top-bar tabs, wrapping at the ends). This is
  scoped to active sessions only.
- **`?`** invokes `/help` when the input is empty, so help is one
  keystroke away from a clean prompt. When the input has content,
  `?` is a literal character.

### Normal mode (input bar focused)

| Key | Action |
|---|---|
| `Enter` | Send message (or run slash command if line starts with `/`) |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+Enter` | Insert newline |
| `Tab` | If `@`- or `/`-autocomplete is open: accept; else: next session tab |
| `Shift+Tab` | If autocomplete is open: previous candidate; else: previous session tab |
| `?` (when input is empty) | Run `/help` |
| `Esc` | If autocomplete is open: dismiss; else: enter Scroll mode |
| `Ctrl+W` | Delete word back |
| `Ctrl+U` | Delete to line start |
| `Ctrl+K` | Delete to line end |
| `Ctrl+A` / `Ctrl+E` | Move to line start / end |
| `Alt+←` / `Alt+→` | Word left / right |
| `Ctrl+C` | Quit |

### Scroll mode

| Key | Action |
|---|---|
| `j` / `k` (or `↓` / `↑`) | Scroll one line |
| `PageUp` / `PageDown` | Scroll 20 lines |
| `c` | Toggle fold on entry under cursor |
| `G` / `i` / printable | Return to bottom and Normal |
| `/` | Search scrollback (incremental) |
| `Esc` | Return to Normal |

### Permission mode (auto-engaged)

| Key | Action |
|---|---|
| `y` | Allow once |
| `n` | Deny |
| `a` | Always allow |

### Inline-picker micro-mode

| Key | Action |
|---|---|
| `↑` / `↓` (or `k` / `j`) | Move cursor |
| `Enter` | Confirm |
| `Esc` | Cancel |
| `Space` (in multi-select pickers) | Toggle selection |

> **Removed vs v1:**
> `Ctrl+B` (sidebar — sidebar is gone),
> `Ctrl+O` (obs — `/obs`),
> `Ctrl+P` (palette — `/`-autocomplete is the palette),
> `Ctrl+I` (import — `/session import`),
> `Ctrl+N` (new tab — `/session new`),
> `Ctrl+1..9` (jump to tab — `/session switch <n>`),
> `Ctrl+Tab` / `Ctrl+Shift+Tab` (MRU — replaced by plain `Tab` / `Shift+Tab`
> across active tabs; deeper history via `/session prev` / `/session next`),
> `Alt+T` / `Alt+A` (session/agent modes — slash commands instead).

## 7. Agent and session statuses

**Agent status** (provider reachability, shared across an agent's
sessions):

| Status | Dot | Meaning |
|---|---|---|
| `Idle` | `○` grey | Discovered on PATH, no live session. |
| `Available` | `◇` blue | Has distribution metadata but binary not yet installed. |
| `Installing { percent }` | `⟳` cyan | Binary download in progress. |
| `Connecting` | `◌` cyan | Spawning subprocess / running ACP handshake. |
| `Connected` | `●` green | At least one session is live and idle. |
| `Busy` | `◎` yellow | At least one session is processing a turn. |
| `Error(msg)` | `✗` red | Spawn or handshake failed. |

**Session status** evolves independently per session:
`Connecting → Connected → Busy → Connected … → Disconnected | Error`.
Reflected in tab badges and the cursor position, not in a status
string.

## 8. Configuration

The TUI reads a `TuiConfig` passed in by the `bitrouter` CLI:

- `listen_addr` — BitRouter's HTTP proxy address. *Not rendered in
  the status bar (which is agent + model only); available via
  `/obs`.*
- `providers` — configured LLM providers (used by `/providers`).
- `route_count` / `daemon_pid` — diagnostic only.
- `agents_dir` — `<bitrouter_home>/agents/` — install root for binary
  agents (one subdir per agent id).
- `agent_state_file` — `<agents_dir>/state.json` — install ledger.
- `cache_dir` — `<bitrouter_home>/cache/` — registry cache and
  per-cwd "import dismissed" markers.

The full agent registry and provider config come from
`bitrouter_config::BitrouterConfig` and are not modified by the TUI.

## 9. Architecture (one screen)

```
EventHandler (mpsc<AppEvent>)
  ├── terminal_event_pump (crossterm async stream)
  └── per-session async tasks
        ├── AcpAgentProvider (one per agent_id, shared)
        │     └── subprocess (ACP/JSON-RPC over stdio)
        └── tagged AppEvent::Session { session_id, agent_id, event } …
```

- `App::run_loop` draws once per event and dispatches keys / agent
  events.
- Session lifecycle (spawn / load / submit / disconnect / respond
  permission) is centralized in `SessionSystem`.
- Render is split per-region: `top_bar` (session tabs), `scrollback`
  (with inline cwd label + input bar), `status_bar`. **No sidebar
  module**, **no modals module** — both are deleted in v3.

## 10. Scope and non-goals

**In scope:**

- Multi-session, multi-agent prompting in a single window.
- Streaming markdown output, tool calls, thinking blocks, inline
  permissions.
- Agent install / discovery / per-cwd import.
- Every session/agent/diagnostic operation as a `/…` slash command
  with inline pickers as needed.

**Not in scope (today):**

- In-TUI auth, login, usage dashboards, or API key management — these
  remain CLI flows.
- Per-project picker — one cwd per process.
- Cross-cwd session import.
- Top-bar token/cost meter (planned, see `specs/multi-session-tui.md`
  PRs 11–13). The status bar's agent slot may extend to show usage
  later, but is intentionally minimal in v3.
- Broadcast prompting (`@all`) — explicitly out of scope.
- Keyboard shortcuts for any operation that has a slash equivalent.

## 11. First-launch experience

1. User runs `bitrouter` in a project directory.
2. The TUI opens with **no active sessions**. The top bar shows just
   the `+` tab. The scrollback is empty except for a one-screen
   onboarding system message:
   ```
   Welcome to BitRouter.

   Try:
     /session new          — spawn a session (opens agent picker)
     /agents               — see what's installed / available
     /help                 — full command reference
   ```
3. In parallel, the TUI scans agent storage for sessions matching the
   launch cwd. If any are found, a one-time toast appends:
   ```
   Found 3 importable session(s) in this cwd. Run /session import.
   ```
   The toast is suppressed on subsequent launches via a per-cwd
   marker file under the cache dir.

## 12. Open questions (for review)

- **Mode disclosure for Scroll.** With no mode label, Scroll mode
  needs a visual cue. Options: tint the input prefix grey, show a
  scroll-position chip (`12/34`) above the input, or do nothing and
  trust that the user remembers they hit `Esc`.
- **`?`-when-empty corner cases.** "Run `/help` when input is empty"
  is unambiguous for a fresh prompt, but what about when the input
  is empty *after* a multi-line edit was just cleared, or after a
  paste that resolved to whitespace? Trim before the empty check, or
  strict-empty?
- **`Tab` order vs MRU.** `Tab` cycles in left-to-right tab order
  today. For users with 4+ tabs that toggle between two of them
  often, MRU would be faster. Worth a follow-up if this proves
  awkward in practice — keep as tab-order for v1.
- **Slash autocomplete trigger position.** The popup currently
  triggers on `/` only when it's the first character of the line.
  Should it also trigger on `/` mid-line (e.g. inside a paste) or
  stay strictly at the start?

---

*If you'd like more depth on any section, or want a separate doc for
internal architecture / contributor onboarding, let me know.*
