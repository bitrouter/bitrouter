# Spec: the chat loop as an explicit machine (steps 2A and 2B)

Status: **draft; §2.3 and §4.2 superseded** · Author: Claude (with Spikel) ·
Date: 2026-09-01

> **Superseded in part (2026-09-02).** §2.3 and §4.2 are written against
> `engine::Session` — `PendingPermission` and a borrowing `Session::prompt`.
> [`ACP_CONTROLLER_AMENDMENT_1.md`](ACP_CONTROLLER_AMENDMENT_1.md) retires that
> type in favour of a shared ACP client, so both sections need rewriting before
> this plan is executed. The state machine itself — `Phase`, `Action`,
> `Effect`, `step()`, and the key table in §1.1 — is unaffected: it is
> transport-agnostic by construction, which is why it survived the change of
> stack underneath it. Per the amendment's §6, this work is step 9 and lands
> last. The picker phase in §3 is deleted rather than ported; see the
> amendment's §5.

Companion to the one-crate refactor discussed on
`claude/bitrouter-tui-architecture-2cc5ce` and to
[PR #848](https://github.com/bitrouter/bitrouter/pull/848), which landed step 0
and step 1. This spec covers **2A** and **2B** only. Steps 3–5 (single journal
ownership, moving the loop into the crate, the `Component` trait) are named
here where they constrain a decision, and are otherwise out of scope.

---

## 1. The problem, stated precisely

`apps/bitrouter/src/chat/session.rs::run` is 706 lines containing **four nested
event loops**:

| loop | lives in | what it owns while it runs |
|---|---|---|
| the session loop | `run`, `'session: loop` | nothing; it is the outer frame |
| the prompt read | `input.rs::read_line` | every key, until Enter |
| the turn loop | `run`, inner `loop` | updates, ticks, permissions, keys, the turn future |
| the picker loop | `pick_provider` | every key, until a choice |
| the permission loop | `answer_permission` | every key, until an answer |

(Five, counting `read_line`. `chat_plain` has a sixth, which is out of scope —
it is a pipe renderer with no keys and no modals.)

**The nesting *is* the state machine.** There is no value anywhere that says
"a turn is in flight" or "a modal owns the keys" — those facts are encoded as
*which function is currently on the stack*. Three consequences follow, and all
three are defects rather than aesthetics:

### 1.1 One key has four meanings and no single place says so

Ctrl-C's behaviour is spread across `editor.rs::apply`, `editor.rs::is_cancel`,
and two hand-written `if key.modifiers.contains(CONTROL)` branches in
`session.rs`. Reconstructed from the source, today's full table is:

| key | idle prompt | during a turn | permission open | picker open |
|---|---|---|---|---|
| `Ctrl-C` | **exit the session** | cancel the turn | deny, turn continues | close, route unchanged |
| `Esc` | *ignored* | cancel the turn | deny, turn continues | close, route unchanged |
| `Ctrl-D` | **exit the session** | ignored | deny (control chord) | close (control chord) |
| `Ctrl-L` | redraw | redraw | **deny** (control chord) | **close** (control chord) |
| `Ctrl-W` | delete word | ignored | deny (control chord) | close (control chord) |
| digit | types the digit | ignored | selects an option | selects a provider |
| Enter | submit | ignored | nothing | nothing |
| other printable | types it | ignored | nothing | nothing |

The two bold cells in the `Ctrl-L` row are almost certainly unintended. Both
modals reject *any* control chord as a cancel, so asking for a redraw while a
permission is up **denies the permission**. Nobody decided that; it fell out of
`if key.modifiers.contains(KeyModifiers::CONTROL) { break deny(); }`. It is
invisible today because the rule lives in two places and neither is next to the
other rows.

This table is the single most valuable thing this refactor produces. §5 makes
it a test.

### 1.2 A modal freezes the transcript

`answer_permission` awaits `stdin.next_event()` and nothing else. While it
runs, `dirty_rx` is not drained and no frame is painted. The agent may still be
streaming — it usually is, since a permission arrives mid-turn — and none of it
appears until the question is answered. The same is true of the picker, which
matters less only because the picker is reachable only from idle.

### 1.3 A permission can outlive its turn and be shown during the next one

`permission_rx` is created **once**, outside `'session`. The turn loop reads it;
the idle prompt does not. So a permission that the agent emits after its turn
has settled — a cancelled turn racing its own tool call, most plausibly — stays
buffered in the channel and is delivered to the **next turn's** select, where it
is drawn as though the current tool call had asked for it.

The existing cancel path partly covers this (`while let Ok(request) =
permission_rx.try_recv() { deny(request); }`) but only on the *cancelled* path.
A turn that ends normally with a late permission in flight has no such drain.

This is a real defect, and §4.4 shows it disappears as a side effect of
flattening rather than needing its own fix.

### 1.4 None of it is testable

`run` is one `async fn` wired to a live `Session`, a real terminal in raw mode,
and a real clock. There is no seam. The two tests that exist in `session.rs`
cover `unanswered()` — a six-line pure function — because it is the only thing
in the file that *can* be tested.

---

## 2. The shape, and where it lives

### 2.1 The reducer

```rust
pub fn step(state: &mut State, action: Action) -> Vec<Effect>;
```

`step` is synchronous, owns no I/O, holds no clock, and never touches the
journal. Everything that awaits, paints, or resolves a protocol request is an
`Effect` the driver runs.

`Vec<Effect>` rather than a fixed-size type: the hot action during streaming is
`Action::Dirty`, which returns `Vec::new()` — and an empty `Vec` does not
allocate. The allocating cases are the rare ones.

### 2.2 It lives in `crates/bitrouter-tui`, from 2A

This is the decision most worth checking, so here is the audit that settles it.
`step`'s vocabulary needs exactly:

| needs | already available in the crate |
|---|---|
| `crossterm::event::Event` | ✅ `crossterm` is a direct dependency |
| `PermissionOption`, `RequestPermissionOutcome`, `ProviderInfo`, `StopReason` | ✅ `agent-client-protocol-schema` |
| `Editor`, `permission::Prompt`, `picker::Picker` | ✅ already in this crate |

It needs **no** `tokio`, **no** `anyhow`, and — see §2.3 — **no**
`bitrouter-sdk`. So `crates/bitrouter-tui/src/machine.rs` adds zero
dependencies, and steps 2A/2B directly serve goal #1 (*all TUI features in one
crate*) instead of deferring it to step 4. Step 4 then has only the async
driver left to move, which is the part that genuinely needs `tokio`.

### 2.3 The machine must not hold a `PendingPermission`

`bitrouter_sdk::acp::up::PendingPermission::new` is `pub(crate)`. Nothing
outside `bitrouter-sdk` can construct one. If `Action::Permission` carried it,
every reducer test touching a permission would be unwritable — which is most of
the tests that matter.

So the payload splits:

- **The machine** holds `permission::Prompt`, a crate type built from
  `(request_id, title, tool_call_id, options)` and freely constructible in a
  test.
- **The driver** holds `HashMap<String, PendingPermission>` keyed by
  `request_id`, and `Effect::Resolve { id, outcome }` is a map lookup plus
  `request.resolve(outcome)`.

`permission::Prompt` gains an `id` field and an `id()` accessor for this. That
is needed anyway once permissions can queue (§4.4) — a queue of prompts with no
identity cannot route its answers.

### 2.4 The `Schedule` stays exactly where it is

`writer::Schedule` already takes the clock as a parameter and is already
tested. `step` emits `Effect::Paint(Trigger)`; the driver does:

```rust
if schedule.wake(trigger, std::time::Instant::now()) {
    view.paint(&shared)?;
}
```

So `step` needs no `Instant` at all — not even on `Action::Tick`. That is the
strongest testability property in the design and it costs nothing.

---

## 3. Step 2A — the vocabulary, and the picker

**Risk: low.** Everything here is reachable only from an idle prompt, where
nothing is streaming and no future is in flight.

### 3.1 Types introduced

```rust
/// What the session is doing, and therefore what a key means.
pub enum Phase {
    /// No turn in flight. Keys go to the line editor.
    Idle,
    /// The route picker owns the keys. Reachable only from `Idle`.
    Routing(Picker),
}

pub struct State {
    pub phase: Phase,
    /// The line being typed.
    pub editor: Editor,
    /// How many prompts this session has sent, so two in a row cannot merge.
    pub prompts: usize,
    /// Whether the agent serves `providers/*` and this session is attributable.
    pub routable: bool,
}

pub enum Action {
    Key(crossterm::event::Event),
    /// stdin ended — the terminal went away.
    InputClosed,
    /// INT / TERM / HUP.
    Signal,
    /// `providers/list` came back.
    ProvidersListed(Vec<ProviderInfo>),
    /// `providers/set` came back, with the route now actually in force.
    Routed(Result<String, String>),
}

pub enum Effect {
    Paint(Trigger),
    Redraw,
    /// Echo the current line. Separate from `Paint` because the driver must
    /// push the buffer into the view before painting it.
    Echo,
    Notice(Notice),
    ClearNotice,
    Modal(Option<Line<'static>>),
    /// Send this line as a turn. `nth` keys the journal chunk.
    Prompt { line: String, nth: usize },
    ListProviders,
    SetProvider(String),
    SetRoute(Option<String>),
    Exit,
}

pub enum Notice {
    Say(String),
    /// The agent's own command list, rendered live from the journal by the
    /// view. Deliberately not a snapshot — see §6.3.
    Commands,
}
```

`Phase::Turn` is **not** introduced in 2A. Nothing would read it: the turn loop
is still nested, so the outer loop never observes a running turn. Adding a
variant with no reader would be dead code.

### 3.2 What changes in the driver

The outer loop stops calling `stdin.read_line(callback)` and dispatches keys
one at a time:

```rust
loop {
    let action = tokio::select! {
        () = shutdown.recv() => Action::Signal,
        event = stdin.next_event() => match event {
            Some(event) => Action::Key(event),
            None => Action::InputClosed,
        },
    };
    for effect in machine::step(&mut state, action) { /* run it */ }
    if exited { break }
}
```

`read_line`, `Echo`, and `Prompt` are **deleted** from `input.rs`. The echo
callback existed only because the line buffer and the screen had different
owners; with the buffer in `State` and the screen behind `Effect::Echo`, there
is one owner and no callback. `input.rs` drops from 145 lines to roughly 85 —
`Stdin::open`, `next_event`, and `Drop`.

`pick_provider` is deleted. Its loop becomes three transitions:

| from | on | to | effects |
|---|---|---|---|
| `Idle` | `Key` submitting `/route`, `routable` | `Idle` | `[ClearNotice, ListProviders]` |
| `Idle` | `Key` submitting `/route`, not `routable` | `Idle` | `[Notice("this session cannot be rerouted…"), Paint(Key)]` |
| `Idle` | `ProvidersListed(ps)` where `Picker::open(true, &ps)` is `Some` | `Routing(p)` | `[Modal(Some(p.render())), Paint(Key)]` |
| `Idle` | `ProvidersListed(ps)` where it is `None` | `Idle` | `[Notice("no routable providers…"), Paint(Key)]` |
| `Routing` | `Key` selecting provider `id` | `Idle` | `[Modal(None), SetProvider(id)]` |
| `Routing` | `Key` cancelling | `Idle` | `[Modal(None), Notice("route unchanged"), Paint(Key)]` |
| `Idle` | `Routed(Ok(in_force))` | `Idle` | `[SetRoute(Some(in_force)), Notice("route: …"), Paint(Key)]` |
| `Idle` | `Routed(Err(e))` | `Idle` | `[Notice("route unchanged: {e}"), Paint(Key)]` |

### 3.3 Effects that await

`ListProviders` and `SetProvider` are `await`ed inline in the effect runner,
exactly as `pick_provider` awaits them today. This is behaviour-preserving, and
it is safe **because these effects are reachable only from `Idle`**, where no
turn is streaming and nothing else needs the loop.

That gives a rule worth writing down and keeping:

> **An effect may `await` only if every phase that emits it is a phase in which
> nothing is in flight.**

2B honours it: `Resolve` is synchronous (`PendingPermission::resolve` takes
`&self` and sends on a oneshot), and `Cancel` is awaited only after the turn
has been abandoned. If a future step needs an awaiting effect from a live
phase, it must spawn and feed the answer back as an `Action` instead.

### 3.4 Behaviour preserved exactly

- `/route` still issues `providers/set` first and re-reads `providers/list`
  afterwards, so what is reported is what is in force, never what was asked for.
- A control chord in the picker still closes it. (§6.1 proposes changing the
  `Ctrl-L` case; that is a 2B decision, not a 2A one.)
- `Esc` at an idle prompt is still ignored.

---

## 4. Step 2B — the turn, and the permission

**Risk: high.** This is where the whole refactor's risk lives. Everything below
is either a new invariant or an intentional behaviour change, and each one is
named.

### 4.1 Phase gains two variants

```rust
pub enum Phase {
    Idle,
    /// A turn is in flight and nothing is modal over it.
    Turn,
    /// A turn is in flight and a permission question owns the keys.
    Answering(Prompt),
    Routing(Picker),
}
```

`Answering` implies a turn. `Routing` implies no turn. Both are reachability
facts today, and encoding them in the enum makes the illegal combinations
unrepresentable rather than merely unreached.

`State` gains:

```rust
/// Questions the agent has asked and nobody has answered yet.
///
/// A deque rather than a slot: once the loop is flat, `permission_rx` is
/// polled while a question is already up, so a second one can arrive with
/// the first still open. Today the channel serialises them by accident —
/// nothing polls it, so they queue in the transport. Making the queue
/// explicit is what stops the second one silently replacing the first.
pub queued: VecDeque<Prompt>,
```

### 4.2 One loop, and the turn future

```rust
let mut turn: Option<Pin<Box<dyn Future<Output = Result<PromptResponse>> + '_>>> = None;

loop {
    let action = tokio::select! {
        () = shutdown.recv()          => Action::Signal,
        event = stdin.next_event()    => match event {
            Some(e) => Action::Key(e),
            None => Action::InputClosed,
        },
        Some(()) = dirty_rx.recv()    => Action::Dirty,
        Some(req) = permissions.next() => Action::Permission(prompt_of(&req), remember(req)),
        result = in_flight(&mut turn) => Action::TurnEnded(result.map(|r| r.stop_reason)
                                                                 .map_err(|e| e.to_string())),
        _ = ticker.tick(), if state.streaming() => Action::Tick,
    };
    for effect in machine::step(&mut state, action) { /* run it */ }
}
```

**The turn future cannot live in `State`.** `State` is plain data that `step`
mutates by `&mut`; a `!Unpin` future being polled across iterations cannot go
there. So it lives in the driver as a boxed future, and the invariant
"`turn.is_some()` iff `phase` is `Turn` or `Answering`" is maintained by exactly
two lines: `Effect::Prompt` sets it, `Action::TurnEnded` and `Effect::Cancel`
clear it.

The cost is one `Box` allocation per turn. The alternative — `tokio::spawn` plus
a oneshot — was rejected because `Session::prompt(&self, ...)` borrows the
session, so spawning would require an `Arc<Session>` and change ownership well
beyond this step.

Awaiting an `Option<F>` without `.unwrap()` uses the same idiom
`signals::Shutdown` already uses for a missing signal registration:

```rust
/// Await the turn if there is one, and never resolve if there is not.
async fn in_flight<F: Future + Unpin>(slot: &mut Option<F>) -> F::Output {
    match slot {
        Some(turn) => turn.await,
        None => std::future::pending().await,
    }
}
```

`Pin<Box<dyn Future>>` is `Unpin`, so `&mut F` is itself a `Future`. Dropping
that borrow when the select loses the race does **not** drop the boxed future,
which is what makes the arm cancel-safe.

### 4.3 The tick arm is gated

Today the ticker is created per turn, so an idle session has no timer. Flat, one
ticker would fire 33 times a second at an idle prompt forever. The `if
state.streaming()` guard means tokio never polls it while idle, so the timer is
never armed and there is no wakeup. `MissedTickBehavior::Delay` makes the first
tick of a new turn immediate, which is wanted.

### 4.4 A permission outside a turn is denied, not queued

```
step(Idle,       Permission(p)) -> [Resolve { id, outcome: unanswered(&p) },
                                    Notice(Say("permission denied: no turn is running")),
                                    Paint(Permission)]
step(Routing(_), Permission(p)) -> same
step(Turn,       Permission(p)) -> [Paint(Permission)]                 // phase -> Answering(p)
step(Answering(_), Permission(p)) -> []                                // pushed to `queued`
```

This is what fixes §1.3. Because the flat loop polls the permission stream in
every phase, a permission that outlives its turn is delivered *at idle* and
denied there, instead of sitting in the transport until the next turn draws it
out of context. The existing `while let Ok(request) = permission_rx.try_recv()`
drain on the cancel path becomes unnecessary and is deleted — anything buffered
arrives on the next iteration and lands in the `Idle` row above, which applies
the same `unanswered()` rule.

`unanswered()` moves from `session.rs` into `permission.rs` as
`Prompt::unanswered(&self) -> RequestPermissionOutcome`, next to the `deny()` it
already calls. Its two existing tests move with it.

### 4.5 Answering, and the queue draining

```
step(Answering(p), Key(digit selecting option o)) ->
    [Resolve { id: p.id(), outcome: Selected(o) },
     Notice(Say("permission answered")), Paint(Permission)]
```

and then, in the same step, if `queued` is non-empty the front is popped and
becomes the new `Answering`; otherwise the phase returns to `Turn`. That
transition is inside `step`, so "the next question comes up by itself" is a
reducer property with a test, not a driver detail.

### 4.6 The `pending_permission` field is deleted from `Journal`

Today `session.rs` writes the open question into the journal
(`set_pending_permission`) purely so that any frame painted by anyone shows it.
With the question in `Phase::Answering`, the view reads it from a
`View::set_permission(Option<Prompt>)` setter instead, driven by
`Effect::ShowPermission`.

`Journal::set_pending_permission` and `Journal::pending_permission` are
**deleted**. The journal goes back to being purely a projection of the ACP
update stream — which is what its own module doc already claims it is — and step
3's job of removing the mutex gets smaller, because one of the two writers of
the journal disappears here.

### 4.7 Cancel-safety audit

Every arm, since a flat select is only correct if all of them are:

| arm | why it is cancel-safe |
|---|---|
| `shutdown.recv()` | `select!` over three `tokio::signal::unix::Signal::recv`, each documented cancel-safe; registered once at install, not per poll |
| `stdin.next_event()` | `UnboundedReceiver::recv` — cancel-safe, and `input.rs` already documents that this is *why* stdin is a task-plus-channel rather than an inline `EventStream` |
| `dirty_rx.recv()` | same |
| `permissions.next()` | `StreamExt::next` on a retained `Pin<Box<dyn Stream>>`; dropping the `Next` future does not consume an item |
| `in_flight(&mut turn)` | see §4.2 — the borrow is dropped, the future is not |
| `ticker.tick()` | `Interval::tick` is cancel-safe |

### 4.8 The select stays **unbiased**

`biased;` was considered and rejected. Under a saturating update stream a
biased select would starve every arm below `dirty_rx` — including the signal
arm. Random selection gives each ready arm a fair share. Determinism is not
needed for tests, because tests drive `step` directly rather than through the
select.

### 4.9 What can be trimmed in the same step (optional, separable)

The permission pump task and its channel exist only to make the stream
`select!`-safe, which `StreamExt::next` on a retained stream already is. Polling
`session.permissions()` directly deletes one `tokio::spawn`, one unbounded
channel, and one `.abort()` at teardown.

Recommended, but **land it as its own commit** so a bisect can separate it from
the flattening.

The update pump is *not* a candidate: it exists because the journal is behind a
mutex and applying updates on the loop's own thread is step 3's decision, not
this one's.

---

## 5. The tests

Five named regression tests, all of them on `step` with no runtime, no
terminal, and no session.

**T1 — the key table.** One table-driven test over (phase × key) asserting the
effects, covering every cell of §1.1. This is the test that makes the four
scattered rules one rule.

**T2 — cancelling with a question outstanding denies it.**
`step(Answering(p), Key(Ctrl-C))` must emit `Resolve` with `p.unanswered()`
*before* anything else, and must not emit `Selected`. Extends the existing
`an_unanswered_permission_takes_the_reject_option` from the function level to
the loop level.

**T3 — Ctrl-C during a permission does not cancel the turn.** Today's behaviour,
and easy to lose while flattening: `answer_permission` consumes the keystroke in
its own loop, so the turn loop's `cancelled` flag never sees it. After
flattening, one careless fallthrough turns "deny this" into "abandon the turn".
`step(Answering(p), Key(Ctrl-C))` must leave the phase at `Turn`, not `Idle`.

**T4 — a second question queues rather than replaces.** Two
`Action::Permission` in a row: the first is shown, the second lands in `queued`,
and answering the first brings the second up. Not possible to write today,
because the shape cannot express it.

**T5 — the transcript keeps streaming while a question is up.**
`step(Answering(_), Action::Dirty)` must not be inert — the driver's schedule
must still go dirty, so the tick paints. This pins §6.2, the one intentional
behaviour change.

Plus **T6 — a permission outside a turn is denied** (§4.4), which is the
regression guard for §1.3.

---

## 6. Behaviour changes, and one open decision

### 6.1 `Ctrl-L` in a modal — **needs your call**

Today it denies the permission / closes the picker, because both modals treat
any control chord as a cancel (§1.1). The flat table makes the cell visible.
Two options:

- **(a) Fix it.** `Ctrl-L` in `Answering` and `Routing` emits `Effect::Redraw`
  and stays. Consistent with every other phase; almost certainly what a user
  expects.
- **(b) Preserve it.** Keep the control-chord-is-cancel rule verbatim, and note
  it in the table as deliberate.

I recommend **(a)**. It is a one-cell change, it is what the rest of the table
already does, and "the redraw key silently denied the agent's request" is a bad
surprise. But it is a behaviour change and it is yours to approve.

### 6.2 The transcript no longer freezes during a permission — intentional

§1.2's freeze goes away as a direct consequence of one select. This is the
change most visible to a user, and it is an improvement: the tool output that
*explains* what the agent is asking permission for now arrives while the
question is up, rather than after it is answered.

### 6.3 `/commands` becomes live rather than a snapshot

Today `/commands` renders the command list at the moment it was typed and
freezes it in the notice. As `Notice::Commands` the view re-renders from the
journal each frame, so an `AvailableCommandsUpdate` arriving afterwards is
reflected. Strictly better, and it falls out of moving the notice from rendered
lines to an enum.

### 6.4 Not in scope, but newly possible

Flattening makes **typing your next prompt while the agent is working** a small
change — `Action::Key` in `Phase::Turn` would feed the editor instead of being
discarded. It is out of scope for 2B because it forces a decision on what
`Ctrl-C` means when there is both a running turn and a half-typed line, and
that decision deserves its own discussion rather than being smuggled in with a
refactor.

---

## 7. What this does *not* do

- **No journal ownership change.** The `Arc<Mutex<Journal>>`, the pump task, the
  dirty channel, and `view::lock` all survive 2A and 2B untouched. That is
  step 3.
- **No move of the driver into the crate.** `session.rs` keeps the async loop,
  `Stdin`, and raw mode. That is step 4, and it is what needs the `tokio`
  dependency question answered.
- **No `Component` trait, no `on_key`.** Both modals still render through the
  view's existing setters. That is step 5.
- **No change to `chat_plain`.** It has no keys and no modals; it is a pipe
  renderer, and the machine has nothing to offer it.
- **No change to the writer, `wrap`, `Schedule`, or any renderer.** The
  differential painting layer is untouched by both steps.

---

## 8. Sequencing and reviewability

| step | commits | what a reviewer checks |
|---|---|---|
| 2A | 1. add `machine.rs` with `Phase::{Idle,Routing}` + tests<br>2. drive the outer loop from it; delete `read_line`/`Echo`/`pick_provider` | that the picker table in §3.2 matches the deleted function line for line |
| 2B | 3. `Prompt` gains `id` + `unanswered`; `Journal::pending_permission` deleted<br>4. flatten: `Phase::{Turn,Answering}`, one select, `in_flight`<br>5. *(optional)* drop the permission pump task | T1–T6, and the cancel-safety table in §4.7 |

Landing 2A alone is worthwhile even if 2B is deferred: it deletes two loops and
the echo callback, and it puts the vocabulary in the crate.
