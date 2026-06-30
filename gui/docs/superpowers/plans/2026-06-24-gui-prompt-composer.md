# Per-Session Prompt Composer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a chat composer to the focused agent's pane so a user can type and send prompts directly to a single ACP session, with the user's prompt echoed into the transcript immediately.

**Architecture:** Two tasks. Task 1 adds the local-echo data path: a `TranscriptItem::UserPrompt` variant (rendered distinctly) plus an `AppModel::append_user_message` local mutation. Task 2 adds the composer UI in `Center` — a `gpui-component` multi-line auto-grow `InputState` whose Enter event (and a Send button) route through a pure `compose_command` helper to echo + `dispatch(SendPrompt{Session{focus}})`.

**Tech Stack:** Rust, gpui + gpui-component (rev `a0ae3a37`), existing `bitrouter-gui` / `bitrouter-gui-core` crates.

**Reference spec:** [`docs/superpowers/specs/2026-06-24-gui-prompt-composer-design.md`](../specs/2026-06-24-gui-prompt-composer-design.md)

**Verified API facts (against the pinned source):**
- `gpui_component::input::InputState`: builders `.multi_line(true)`, `.auto_grow(min_rows, max_rows)`, `.placeholder(impl Into<SharedString>)`; `.value() -> SharedString`; `.set_value("", window, cx)` (already used in `broadcast.rs`).
- `gpui_component::input::InputEvent { Change, PressEnter { secondary: bool, shift: bool }, Focus, Blur }`. In multi-line mode, plain Enter emits `PressEnter` **without** inserting a newline; Shift+Enter inserts the newline.
- `Context::subscribe_in(&entity, window, |this, _emitter, event: &InputEvent, window, cx| {…}) -> Subscription` (gpui `app/context.rs:355`). `InputState` implements `EventEmitter<InputEvent>`.
- `Input::new(&input_entity)` renders the bound input (see `broadcast.rs`).
- `cx.entity()` returns `Entity<Center>` for button closures (pattern from `broadcast.rs`).

---

## File Structure

- **Modify** `crates/bitrouter-gui-core/src/state.rs` — add `TranscriptItem::UserPrompt { text }`.
- **Modify** `crates/bitrouter-gui/src/views/transcript.rs` — render `UserPrompt` distinctly (the only exhaustive `TranscriptItem` match).
- **Modify** `crates/bitrouter-gui/src/app_model.rs` — `append_user_message`.
- **Modify** `crates/bitrouter-gui/src/views/center.rs` — `compose_command` pure helper + composer input, subscription, `send_prompt`, and the composer bar.

---

## Task 1: Local echo — `UserPrompt` transcript item + `append_user_message`

**Files:**
- Modify: `crates/bitrouter-gui-core/src/state.rs`
- Modify: `crates/bitrouter-gui/src/views/transcript.rs`
- Modify: `crates/bitrouter-gui/src/app_model.rs`

- [ ] **Step 1: Write failing tests for `append_user_message`**

Add to the `tests` module in `crates/bitrouter-gui/src/app_model.rs` (it already imports `SessionId`, `MockFeed`, `AppModel`, `gpui::{AppContext as _, TestAppContext}`):

```rust
#[gpui::test]
fn append_user_message_pushes_user_prompt(cx: &mut TestAppContext) {
    use bitrouter_gui_core::state::TranscriptItem;
    let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
    cx.run_until_parked();

    let id = SessionId("auth-fix".into());
    model.update(cx, |m, _| m.append_user_message(&id, "hello agent".into()));

    let last = model.read_with(cx, |m, _| {
        m.state.session("auth-fix").and_then(|v| v.transcript.last().cloned())
    });
    assert!(matches!(last, Some(TranscriptItem::UserPrompt { text }) if text == "hello agent"));
}

#[gpui::test]
fn append_user_message_unknown_id_is_noop(cx: &mut TestAppContext) {
    let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
    cx.run_until_parked();

    let total = |m: &AppModel| m.state.sessions.iter().map(|v| v.transcript.len()).sum::<usize>();
    let before = model.read_with(cx, |m, _| total(m));
    model.update(cx, |m, _| m.append_user_message(&SessionId("ghost".into()), "x".into()));
    let after = model.read_with(cx, |m, _| total(m));
    assert_eq!(before, after);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p bitrouter-gui append_user_message 2>&1 | tail -20`
Expected: FAIL — `no method named append_user_message` / `no variant UserPrompt`.

- [ ] **Step 3: Add the `UserPrompt` variant**

In `crates/bitrouter-gui-core/src/state.rs`, add a variant to the `TranscriptItem` enum (after `ToolCall`):

```rust
    /// A prompt the user typed, echoed locally on send. Never produced by
    /// `reduce` from a feed `Event` — only by `AppModel::append_user_message`.
    UserPrompt {
        text: String,
    },
```

- [ ] **Step 4: Render `UserPrompt` distinctly in the transcript**

In `crates/bitrouter-gui/src/views/transcript.rs`:

First add `h_flex` to the `gpui_component` import (it currently imports `scroll::ScrollableElement, v_flex, ActiveTheme, StyledExt`):

```rust
use gpui_component::{h_flex, scroll::ScrollableElement, v_flex, ActiveTheme, StyledExt};
```

Then add a new match arm inside the `.map(|item| match item { … })` (after the `ToolCall` arm). It right-aligns a secondary-background bubble so user turns are visually distinct from left-aligned agent text:

```rust
            TranscriptItem::UserPrompt { text } => h_flex()
                .w_full()
                .px_3()
                .py_1()
                .justify_end()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(text.clone()),
                )
                .into_any_element(),
```

Also update the module doc comment list near the top of the file to mention `UserPrompt → right-aligned user bubble` (one line, alongside the existing `Message`/`Thought`/`ToolCall` bullets).

- [ ] **Step 5: Implement `append_user_message`**

In `crates/bitrouter-gui/src/app_model.rs`, add this method to the `impl AppModel` block (next to `resolve_pending`):

```rust
    /// Append a user-typed prompt to `id`'s transcript as a local echo. No feed
    /// traffic — mirrors `resolve_pending`'s local-mutation pattern. No-op for an
    /// unknown id.
    pub fn append_user_message(&mut self, id: &SessionId, text: String) {
        use bitrouter_gui_core::state::TranscriptItem;
        if let Some(v) = self.state.sessions.iter_mut().find(|v| &v.session.id == id) {
            v.transcript.push(TranscriptItem::UserPrompt { text });
        }
    }
```

(`SessionId` is already imported in `app_model.rs`.)

- [ ] **Step 6: Run tests + build**

Run: `cargo test -p bitrouter-gui append_user_message 2>&1 | tail -20 && cargo test -p bitrouter-gui-core 2>&1 | tail -8 && cargo build -p bitrouter-gui 2>&1 | tail -8`
Expected: the two new tests PASS; core tests still PASS; GUI builds (the new `transcript.rs` arm makes the exhaustive match compile).

- [ ] **Step 7: Commit**

```bash
git add crates/bitrouter-gui-core/src/state.rs crates/bitrouter-gui/src/views/transcript.rs crates/bitrouter-gui/src/app_model.rs
git commit -m "feat(gui): UserPrompt transcript item + append_user_message local echo"
```

---

## Task 2: Composer UI in `Center` — input, Enter/Send → echo + SendPrompt

**Files:**
- Modify: `crates/bitrouter-gui/src/views/center.rs`

- [ ] **Step 1: Write failing tests for `compose_command`**

Add to the `tests` module in `crates/bitrouter-gui/src/views/center.rs`:

```rust
    use super::compose_command;
    use bitrouter_gui_core::protocol::{Command, SessionId, Target};

    #[test]
    fn compose_command_focus_and_text_builds_session_prompt() {
        let id = SessionId("s1".into());
        let cmd = compose_command(Some(&id), "  hi  ");
        match cmd {
            Some(Command::SendPrompt { target: Target::Session { id }, text }) => {
                assert_eq!(id.0, "s1");
                assert_eq!(text, "hi"); // trimmed
            }
            other => panic!("expected Session SendPrompt, got {other:?}"),
        }
    }

    #[test]
    fn compose_command_no_focus_is_none() {
        assert!(compose_command(None, "hi").is_none());
    }

    #[test]
    fn compose_command_blank_text_is_none() {
        let id = SessionId("s1".into());
        assert!(compose_command(Some(&id), "   ").is_none());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p bitrouter-gui compose_command 2>&1 | tail -20`
Expected: FAIL — `cannot find function compose_command`.

- [ ] **Step 3: Add the pure `compose_command` helper**

In `crates/bitrouter-gui/src/views/center.rs`, add near the top (after the imports, before `struct Center`):

```rust
/// Build the `SendPrompt` command for the focused session, or `None` when there
/// is no focus or the text is blank. Pure so the send branching is testable
/// without driving gpui events.
pub fn compose_command(
    focus: Option<&SessionId>,
    text: &str,
) -> Option<bitrouter_gui_core::protocol::Command> {
    use bitrouter_gui_core::protocol::{Command, Target};
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

- [ ] **Step 4: Run to verify the helper passes**

Run: `cargo test -p bitrouter-gui compose_command 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Add composer fields + imports to `Center`**

In `crates/bitrouter-gui/src/views/center.rs`:

Extend the `gpui` import to include `Subscription`:

```rust
use gpui::{
    div, prelude::FluentBuilder as _, AppContext as _, ClickEvent, Context, Entity, IntoElement,
    ParentElement, Render, Styled, Subscription, Window,
};
```

Extend the `gpui_component` import to bring in the input types:

```rust
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme, StyledExt,
};
```

Add two fields to `struct Center`:

```rust
pub struct Center {
    model: Entity<AppModel>,
    /// One `TerminalView` entity per session, lazily constructed on first focus.
    terminal_cache: HashMap<SessionId, Entity<TerminalView>>,
    /// Lazily-created prompt composer (shared across focus; ACP mode only).
    composer: Option<Entity<InputState>>,
    /// Retains the composer's `PressEnter` subscription (drops if not held).
    _subscriptions: Vec<Subscription>,
}
```

Initialise them in `new`:

```rust
        Self {
            model,
            terminal_cache: HashMap::new(),
            composer: None,
            _subscriptions: Vec::new(),
        }
```

- [ ] **Step 6: Add the `send_prompt` method**

Add to `impl Center` (after `terminal_for`):

```rust
    /// Echo the composer's text into the focused session and dispatch a
    /// `SendPrompt`, then clear the input. No-op when blank or unfocused.
    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .composer
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        let focus = self.model.read(cx).state.focus.clone();
        let Some(cmd) = compose_command(focus.as_ref(), &text) else {
            return;
        };
        // `compose_command` returned `Some`, so focus is `Some` and text non-blank.
        let focus_id = focus.expect("focus present when compose_command is Some");
        let echo = text.trim().to_string();
        self.model.update(cx, |m, _| {
            m.append_user_message(&focus_id, echo);
            m.dispatch(cmd);
        });
        if let Some(input) = &self.composer {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }
```

- [ ] **Step 7: Lazily build the composer + subscription, and render the bar**

In `render`, first rename the unused window param: change `fn render(&mut self, _window: &mut Window, …)` to `fn render(&mut self, window: &mut Window, …)`.

Then, immediately **after** the `let body: gpui::AnyElement = match render_mode { … };` block and **before** the permission-modal section, insert:

```rust
        // ── Prompt composer (ACP mode only) ───────────────────────────────
        let composer_bar = if matches!(render_mode, RenderMode::Acp) {
            // Lazily create the input + its PressEnter subscription.
            if self.composer.is_none() {
                let placeholder = format!("Message {name}…");
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .multi_line(true)
                        .auto_grow(1, 6)
                        .placeholder(placeholder)
                });
                let sub = cx.subscribe_in(
                    &input,
                    window,
                    |this: &mut Center, _input, event: &InputEvent, window, cx| {
                        // Plain Enter sends; Shift+Enter inserts a newline (handled
                        // by the component, so we ignore shift==true).
                        if let InputEvent::PressEnter { shift: false, .. } = event {
                            this.send_prompt(window, cx);
                        }
                    },
                );
                self.composer = Some(input);
                self._subscriptions.push(sub);
            }

            let input_entity = self.composer.clone();
            input_entity.map(|input| {
                let this_entity = cx.entity().clone();
                let send_btn = Button::new(gpui::ElementId::Name("composer-send".into()))
                    .label("Send")
                    .on_click(move |_: &ClickEvent, window, cx| {
                        this_entity.update(cx, |c, cx| c.send_prompt(window, cx));
                    });
                h_flex()
                    .w_full()
                    .p_2()
                    .gap_x_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .items_end()
                    .child(div().flex_1().child(Input::new(&input)))
                    .child(send_btn)
            })
        } else {
            None
        };
```

Then change the `content` composition to include the bar between `body` and the modal. Replace:

```rust
        let content = v_flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(cx.theme().background)
            .child(header)
            .child(body);
```

with:

```rust
        let content = v_flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(cx.theme().background)
            .child(header)
            .child(body)
            .when_some(composer_bar, |el, bar| el.child(bar));
```

- [ ] **Step 8: Build + run the existing Center smoke tests**

Run: `cargo build -p bitrouter-gui 2>&1 | tail -20 && cargo test -p bitrouter-gui center:: 2>&1 | tail -15`
Expected: compiles; `center_renders_without_panic` and `center_constructible_with_focused_session` still PASS (the scenario auto-focuses a session in `Terminal` mode by default, so the composer branch is exercised only when a session is in ACP mode — both paths must compile and not panic).

- [ ] **Step 9: Add a Center smoke test that exercises the ACP composer path**

Add to the `tests` module in `center.rs`:

```rust
    /// Force the focused session into ACP mode so the composer branch builds and
    /// renders without panic.
    #[gpui::test]
    fn center_acp_mode_builds_composer_without_panic(cx: &mut TestAppContext) {
        use bitrouter_gui_core::protocol::RenderMode;
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let focused = model.read_with(cx, |m, _| m.state.focus.clone()).expect("focus");
        model.update(cx, |m, _| m.set_render_mode(&focused, RenderMode::Acp));

        cx.update(|cx| {
            let _ = cx.new(|cx| Center::new(model.clone(), cx));
        });
    }
```

Run: `cargo test -p bitrouter-gui center:: 2>&1 | tail -15`
Expected: all `center::` tests PASS (a full render is driven by gpui's test harness; if this harness does not invoke `render`, the test still validates construction without panic — that is acceptable coverage for the wiring, since `compose_command` and `append_user_message` carry the logic coverage).

- [ ] **Step 10: Commit**

```bash
git add crates/bitrouter-gui/src/views/center.rs
git commit -m "feat(gui): per-session prompt composer (Enter/Send -> echo + SendPrompt)"
```

---

## Self-Review (completed by plan author)

- **Spec coverage:** §6.1 `UserPrompt` → Task 1 Step 3; §6.2 `append_user_message` → Task 1 Step 5; §6.3 composer/subscription/send/visibility → Task 2 Steps 5–7; §6.4 distinct render → Task 1 Step 4; §7 data flow → Task 2 Step 6 (`send_prompt`); §8 `compose_command` → Task 2 Step 3; §9 testing → Task 1 Steps 1/6 + Task 2 Steps 1/9. Non-goals (no protocol/reduce change, no turn-gating, broadcast unchanged) are respected — no task touches `protocol.rs`, `reduce`, or `broadcast.rs`.
- **Placeholder scan:** none — every code step is complete and concrete. The one honest hedge (Task 2 Step 9: whether the gpui test harness drives a full `render`) is explained, not a TODO; logic coverage lives in the pure/state tests regardless.
- **Type consistency:** `compose_command(Option<&SessionId>, &str) -> Option<Command>` is defined in Task 2 Step 3 and called identically in `send_prompt` (Step 6) and the tests (Step 1). `append_user_message(&SessionId, String)` defined in Task 1 Step 5, called in Task 2 Step 6 and tested in Task 1 Step 1. `TranscriptItem::UserPrompt { text }` defined in Task 1 Step 3, matched in Step 4, asserted in Step 1. `InputEvent::PressEnter { shift, .. }` and `subscribe_in` match the verified signatures.
