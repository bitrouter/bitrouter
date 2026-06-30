# BitRouter GUI — Per-Session Prompt Composer — Design Spec

- **Date:** 2026-06-24
- **Status:** Draft — pending review
- **Topic:** Add a per-session prompt composer (chat input) to the focused
  agent's pane so a user can type and send prompts to a single ACP session
  directly, without ⌘K or the multi-select broadcast bar.
- **Related:** [`2026-06-24-gui-acp-client-design.md`](2026-06-24-gui-acp-client-design.md)
  (the ACP client this composer drives).

---

## 1. Problem

The GUI is a real ACP client: a focused session renders a live transcript and
answers permission prompts, and `AcpFeed` handles `Command::SendPrompt`. But the
UI offers **no direct way to send a prompt to the focused agent**. The only input
paths are:

- **⌘K command palette** — routes text through an LLM intent-parser
  (`GenAiIntentClient`) that calls BitRouter's OpenAI-compatible endpoint at
  `localhost:4356/v1/` with model `claude/claude-sonnet-4-5`. Requires a
  configured router model; fails on a default `providers: {}` config.
- **Broadcast bar** ([`broadcast.rs`](../../../crates/bitrouter-gui/src/views/broadcast.rs)) —
  only appears when one or more agents are *selected* (via ⌘-click), and targets
  the selection.

Neither is a discoverable "type to the focused agent" affordance, so a freshly
spawned single session looks unusable. This spec adds that affordance.

## 2. Goals

- A composer pinned to the bottom of the **focused** session's pane, in **ACP
  render mode only**, that sends `SendPrompt { target: Session { focus } }`.
- Chat-composer UX: Enter sends, Shift+Enter inserts a newline, auto-growing
  textarea, plus a "Send" button fallback.
- **Local echo**: the user's prompt appears in the transcript immediately,
  rendered distinctly from agent messages.
- Built on the existing `gpui-component` `Input`/`InputState` primitive — no
  hand-rolled key handling.

## 3. Non-Goals

- Per-session draft retention (one shared composer; in-progress text is
  ephemeral).
- Turn-in-flight gating / disabling the input while the agent is responding.
- Replacing or changing the ⌘K palette or the broadcast bar (both stay as-is).
- Terminal-mode input (the PTY already handles its own input).

## 4. Decisions (resolved during brainstorming)

| Decision | Choice |
| --- | --- |
| Where the composer lives | Dedicated composer at the bottom of the `Center` pane (per-focused-agent), broadcast bar unchanged |
| Send affordance | Enter sends, Shift+Enter newline, plus a Send button — native to `gpui-component` |
| Local echo | Yes — a distinct user-message `TranscriptItem` variant + a local `AppModel` method |

## 5. `gpui-component` primitive (verified, rev `a0ae3a37`)

`InputState` (in `crates/ui/src/input/state.rs`) provides exactly what's needed:

- `InputState::new(window, cx).multi_line(true).auto_grow(min_rows, max_rows).placeholder(…)`
  — an auto-growing multi-line textarea.
- Emits `InputEvent::PressEnter { secondary: bool, shift: bool }`. In multi-line
  mode, **plain Enter fires `PressEnter` without inserting a newline; Shift+Enter
  inserts the newline** (component-native chat behavior).
- `.value()` reads the text; `.set_value("", window, cx)` clears it.

Subscribe with `cx.subscribe(&input, handler)` and act on
`PressEnter { shift: false, .. }`. This mirrors how
[`broadcast.rs`](../../../crates/bitrouter-gui/src/views/broadcast.rs) and
[`command_palette.rs`](../../../crates/bitrouter-gui/src/views/command_palette.rs)
already use `InputState`.

## 6. Components & changes

### 6.1 Core (`bitrouter-gui-core`) — local echo only, no feed/protocol change

- **`state.rs`**: add `TranscriptItem::UserPrompt { text: String }`. The echo is
  local view state produced by the GUI, never by `reduce` from a feed `Event`, so
  `protocol.rs`, `Event`, and `reduce` are untouched. (`translate` already maps
  ACP `UserMessageChunk → None`, so the agent cannot double-echo the user's text.)

### 6.2 `app_model.rs`

- Add `append_user_message(&mut self, id: &SessionId, text: String)` — pushes a
  `TranscriptItem::UserPrompt { text }` onto the matching session's transcript.
  Same local-mutation pattern as `resolve_pending` / `set_focus`; no feed traffic.

### 6.3 `views/center.rs`

- `Center` gains `composer: Option<Entity<InputState>>` and
  `_subscriptions: Vec<gpui::Subscription>`. Rename the currently-unused
  `_window` param of `render` to `window` (needed by `InputState::new`).
- Lazily create the composer the first time `render` runs with a window:
  `InputState::new(window, cx).multi_line(true).auto_grow(1, 6).placeholder(format!("Message {name}…"))`,
  and `cx.subscribe(&input, …)` storing the subscription. The subscription handles
  `InputEvent::PressEnter { shift: false, .. }` → `send`.
- The composer bar (the `Input` + a "Send" `Button`) is appended **after the
  transcript body, only when a session is focused and `render_mode == Acp`**.
- `send`: read the input value; compute the command via the pure helper
  `compose_command` (below); if `Some`, `model.update(|m, _| { m.append_user_message(focus, text.clone()); m.dispatch(cmd); })`, then clear the input. The focus is read from `state.focus` at send time (dynamic), so it always targets the currently-focused agent.

### 6.4 `views/transcript.rs`

- Render `TranscriptItem::UserPrompt { text }` distinctly from agent
  `Message`/`Thought`/`ToolCall` — e.g. right-aligned with an accent background /
  a "you" affordance — so user vs agent turns are visually distinguishable.

## 7. Data flow (send)

```
type → Enter (PressEnter{shift:false})  OR  click "Send"
  → text = input.value()
  → cmd = compose_command(state.focus, &text)          // None if no focus / empty
  → if Some(cmd):
        model.update:
            append_user_message(focus, text)            // local echo, instant
            dispatch(cmd)  // SendPrompt{target: Session{focus}, text} → AcpFeed
        input.set_value("")                             // clear
  → agent reply streams back as MessageChunk → reduce coalesces → transcript
```

## 8. Pure helper (testability seam)

```rust
/// Build the SendPrompt command for the focused session, or None when there is
/// no focus or the text is blank. Keeps the send decision unit-testable without
/// gpui events.
pub fn compose_command(focus: Option<&SessionId>, text: &str) -> Option<Command> {
    let trimmed = text.trim();
    match (focus, trimmed.is_empty()) {
        (Some(id), false) => Some(Command::SendPrompt {
            target: Target::Session { id: id.clone() },
            text: trimmed.to_string(),
        }),
        _ => None,
    }
}
```

The `PressEnter` subscription and button both route through `send`, which routes
through `compose_command` — so the branching logic is covered by pure tests and
only the thin gpui wiring is untested.

## 9. Testing

- **Pure:** `compose_command` — focus + non-empty → `Some(SendPrompt{Session})`;
  no focus → `None`; blank/whitespace → `None`.
- **State:** `append_user_message` appends a `UserPrompt` to the right session and
  is a no-op for an unknown id.
- **View smoke:** `transcript.rs` renders a transcript containing a `UserPrompt`
  without panic; `center.rs` renders a focused ACP session (composer builds)
  without panic.

## 10. Files touched

- `crates/bitrouter-gui-core/src/state.rs` — `TranscriptItem::UserPrompt`.
- `crates/bitrouter-gui/src/app_model.rs` — `append_user_message`.
- `crates/bitrouter-gui/src/views/center.rs` — composer + subscription + `send` +
  `compose_command`.
- `crates/bitrouter-gui/src/views/transcript.rs` — render `UserPrompt`.

## 11. Risks / open questions

- **Subscription lifetime:** the `InputState` subscription must be retained in
  `Center._subscriptions` or it drops immediately; created once alongside the
  lazy input init.
- **Shared composer across focus switches:** in-progress text persists when
  switching focus (acceptable for v1; per-session drafts are a non-goal).
- **Mid-turn sends:** allowed; if the agent rejects a concurrent
  `session/prompt`, `AcpFeed` already surfaces "prompt failed: …" in the
  transcript. No turn-state gating in v1.
