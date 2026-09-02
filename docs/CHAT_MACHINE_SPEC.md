# Spec: the chat loop as an explicit machine (steps 2A and 2B)

Status: **accepted; §2.3 and §4.2 re-derived against the shared client** ·
Author: Claude (with Spikel) · Date: 2026-09-01, revised 2026-09-02

> **Revised (2026-09-02), and the revision is the reason to read this note.**
> §2.3 and §4.2 were written against `engine::Session` — a `PendingPermission`
> in `acp::up`, and a `Session::prompt(&self)` that borrowed the session.
> [`ACP_CONTROLLER_AMENDMENT_1.md`](ACP_CONTROLLER_AMENDMENT_1.md) retired that
> stack for `bitrouter_sdk::acp::client::AcpClient`, so both sections were
> re-derived from what is actually there before this plan was executed. **Both
> conclusions survived, for different reasons than the ones originally given**
> — see the "re-derived" notes in each section.
>
> The state machine itself — `Phase`, `Action`, `Effect`, `step()`, and the key
> table in §1.1 — was unaffected: it is transport-agnostic by construction,
> which is why it survived the change of stack underneath it.
>
> **One structural correction.** The original draft cut `Phase::Routing`,
> because the amendment's §5 was read as deleting the picker. It does not:
> §5 deletes `SessionProviders` and manager-side `providers/*`, and says
> explicitly that `picker::Picker` *survives* with its `available` parameter
> re-pointed at the controller's three-condition `routeControl` gate. Step 6
> rebuilt `/route` on `_bitrouter/route/*` and it is live. The machine
> therefore has **four** phases, not three: `Idle`, `Turn`, `Answering`,
> `Routing`. §3 is a port, not a deletion.
>
> Per the amendment's §6, this work is step 9 and lands last.

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
| `Ctrl-L` | redraw | redraw | ~~**deny** (control chord)~~ → redraw, stays | ~~**close** (control chord)~~ → redraw, stays |
| `Ctrl-W` | delete word | ignored | deny (control chord) | close (control chord) |
| digit | types the digit | ignored | selects an option | selects a provider |
| Enter | submit | ignored | nothing | nothing |
| other printable | types it | ignored | nothing | nothing |

The two struck cells in the `Ctrl-L` row were unintended. Both modals rejected
*any* control chord as a cancel, so asking for a redraw while a permission was
up **denied the permission**. Nobody decided that; it fell out of
`if key.modifiers.contains(KeyModifiers::CONTROL) { break deny(); }`. It was
invisible because the rule lived in two places and neither was next to the
other rows. §6.1 is the change; it landed after the table was pinned verbatim,
in its own commit.

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
| `PermissionOption`, `PermissionOptionId`, `RequestPermissionOutcome`, `StopReason` | ✅ `agent-client-protocol-schema` |
| `Editor`, `permission::Prompt`, `picker::Picker`, `writer::Trigger` | ✅ already in this crate |
| `ratatui::text::Line`, for the modal row a phase hands the view | ✅ `ratatui` |

**Re-checked after the migration (2026-09-02).** The route plane replaced
`providers/*`, so the row that used to read `ProviderInfo` is gone — and what
replaced it is *less* demanding, not more: `_bitrouter/route/list` yields
`Vec<String>` and an `Option<String>`, which are not types at all. The one
new question the shared client raises is whether the reducer needs
`RouteError`; it does not, because the driver renders an error to a string
before it ever becomes an `Action` (§3.2). Likewise `StopReason` reaches the
reducer but `PromptResponse` does not: `report_turn` is the driver's, and it
runs on the round-trip before the action is built.

It needs **no** `tokio`, **no** `anyhow`, and — see §2.3 — **no**
`bitrouter-sdk`. So `crates/bitrouter-tui/src/machine.rs` adds zero
dependencies, and steps 2A/2B directly serve goal #1 (*all TUI features in one
crate*) instead of deferring it to step 4. Step 4 then has only the async
driver left to move, which is the part that genuinely needs `tokio`.

### 2.3 The machine must not hold a `PendingPermission`

**Re-derived 2026-09-02.** The type moved — it is
`bitrouter_sdk::acp::client::PendingPermission` now — and its shape changed:
it grew a public `request_id`, its `tool_call`/`options` fields are public, and
it carries a shared `Arc<PermissionResolver>` so `resolve(&self)` is
idempotent across clones. Three of the four things that made the original
argument have therefore gone. The argument still holds, on the one that did
not, plus one the shared client added:

1. **`PendingPermission::new` is still `pub(crate)`.** Every field a test would
   need is public, but there is no way to *build* one from outside
   `bitrouter-sdk`. A reducer test that had to construct a permission would be
   unwritable — which is most of the tests that matter (T2, T3, T4, T6).
2. **`bitrouter-tui` does not depend on `bitrouter-sdk`, and must not start.**
   This is new since the original draft: under the engine, the reducer's home
   crate was an open question, and the type's constructor was the whole
   argument. Now the machine's home is settled (§2.2) and holding a
   `PendingPermission` would add an SDK edge to the renderer purely to carry a
   resolver the reducer must never call. A resolver is I/O; `step` owns none.

So the payload splits, exactly as originally drawn:

- **The machine** holds `permission::Prompt`, a crate type built from
  `(id, title, tool_call_id, options)` and freely constructible in a test.
- **The driver** holds `HashMap<String, PendingPermission>` keyed by
  `PendingPermission::request_id`, and `Effect::Resolve { id, outcome }` is a
  map lookup plus `request.resolve(outcome)`.

`permission::Prompt` gains an `id` field and an `id()` accessor for this, fed
from `PendingPermission::request_id` — which is the one piece of the new shape
that makes the split *cheaper* than it was, since the id no longer has to be
minted by the driver. It is needed anyway once permissions can queue (§4.4):
a queue of prompts with no identity cannot route its answers.

**Removing an entry is the driver's, and it must not be skipped.** `resolve`
is idempotent, so a stale entry cannot double-answer — but it holds a strong
`Arc` on the resolver, and the client's ledger holds only a weak one *on
purpose*, so that a dropped request still denies itself (I1's drop arm). A
driver that never removed answered entries would keep every request of the
session alive and quietly disable that arm. So `Effect::Resolve` **removes**
rather than looks up, and `Effect::Cancel` clears the map.

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
    /// Whether the controller advertised `_bitrouter/route/list` **and**
    /// `_bitrouter/route/set` for this session. A controller with no trusted
    /// local binding advertises neither, and `/route` then says so rather
    /// than opening a picker that cannot act.
    pub routable: bool,
}

pub enum Action {
    Key(crossterm::event::Event),
    /// stdin ended — the terminal went away.
    InputClosed,
    /// INT / TERM / HUP.
    Signal,
    /// `_bitrouter/route/list` came back, or failed with a rendered message.
    Routes(Result<Routes, String>),
    /// `_bitrouter/route/set` came back, with the route now actually in force.
    Routed(Result<String, String>),
}

/// What `_bitrouter/route/list` reported: the routes on offer, and the lease
/// the daemon says is in force.
pub struct Routes {
    pub available: Vec<String>,
    pub current: Option<String>,
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
    ListRoutes,
    SetRoute(String),
    /// The route the footer names for the rest of the session — the one the
    /// daemon confirmed, never the one that was asked for.
    RouteInForce(Option<String>),
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

`pick_route` is deleted. Its loop becomes these transitions:

| from | on | to | effects |
|---|---|---|---|
| `Idle` | `Key` submitting `/route`, `routable` | `Idle` | `[Echo, ClearNotice, ListRoutes]` |
| `Idle` | `Key` submitting `/route`, not `routable` | `Idle` | `[Echo, ClearNotice, Notice("this session cannot be rerouted…"), Paint(Key)]` |
| `Idle` | `Routes(Ok(r))` where `Picker::open(routable, …)` is `Some` | `Routing(p)` | `[Modal(Some(p.render())), Paint(Key)]` |
| `Idle` | `Routes(Ok(r))` where it is `None` | `Idle` | `[Notice("no routes to choose between"), Paint(Key)]` |
| `Idle` | `Routes(Err(e))` | `Idle` | `[Notice("route unchanged: {e}"), Paint(Key)]` |
| `Routing` | `Key` selecting route `r` | `Idle` | `[Modal(None), SetRoute(r)]` |
| `Routing` | `Key` cancelling | `Idle` | `[Modal(None), Notice("route unchanged"), Paint(Key)]` |
| `Idle` | `Routed(Ok(in_force))` | `Idle` | `[RouteInForce(Some(in_force)), Notice("route: …"), Paint(Key)]` |
| `Idle` | `Routed(Err(message))` | `Idle` | `[Notice(message), Paint(Key)]` |

`Routed(Err)` carries a message the driver already rendered, rather than a
`RouteError`: a refused route, a vanished binding, and a transport failure read
differently, and the branch that knows the difference is the one that holds the
typed error. Keeping the rendering in the driver is what keeps `bitrouter-sdk`
out of the reducer's vocabulary (§2.2).

### 3.3 Effects that await

`ListRoutes` and `SetRoute` are `await`ed inline in the effect runner, exactly
as `pick_route` awaits them today, and their answers are fed back as
`Action::Routes` / `Action::Routed` on the next pass. This is
behaviour-preserving, and it is safe **because these effects are reachable only
from `Idle`**, where no turn is streaming and nothing else needs the loop.

That gives a rule worth writing down and keeping:

> **An effect may `await` only if every phase that emits it is a phase in which
> nothing is in flight.**

2B honours it: `Resolve` is synchronous (`PendingPermission::resolve` takes
`&self` and sends on a oneshot), and `Cancel` is awaited only after the turn
has been abandoned — and what it awaits, `AcpClient::cancel`, is an
`unbounded_send` behind an `async fn` and never actually parks. If a future
step needs an awaiting effect from a live phase, it must spawn and feed the
answer back as an `Action` instead.

### 3.4 Behaviour preserved exactly

- `/route` still reports what `_bitrouter/route/set` **confirmed**, never what
  was asked for, and a `set` that fails leaves the old route marked and says
  why.
- The picker is still gated on the controller's three-condition `routeControl`
  capability, asked in two places that must agree: `routable` on the state, and
  `Picker::open`'s `available`.
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
    // An effect that awaited inline may have produced an answer; it is
    // dispatched before the select is entered again.
    let action = match queued_actions.pop_front() {
        Some(action) => action,
        None => tokio::select! {
            () = shutdown.recv()          => Action::Signal,
            event = stdin.next_event()    => match event {
                Some(e) => Action::Key(e),
                None => Action::InputClosed,
            },
            Some(()) = dirty_rx.recv()    => Action::Dirty,
            Some(req) = permissions.next() => Action::Permission(remember(req)),
            result = in_flight(&mut turn) => { turn = None; ended(result) }
            _ = ticker.tick(), if state.streaming() => Action::Tick,
        },
    };
    for effect in machine::step(&mut state, action) { /* run it */ }
    if exited { break }
}
```

**The turn future cannot live in `State`.** `State` is plain data that `step`
mutates by `&mut`; a `!Unpin` future being polled across iterations cannot go
there. So it lives in the driver as a boxed future, and the invariant
"`turn.is_some()` iff `phase` is `Turn` or `Answering`" is maintained by exactly
two lines: `Effect::Prompt` sets it, and the `TurnEnded` arm plus
`Effect::Cancel` clear it.

**Re-derived 2026-09-02: the idiom survives, and one of the two reasons for it
did not.**

`Session::prompt(&self, …)` became `AcpClient::prompt(&self, session_id, text)`.
The *shape* the original argument turned on is unchanged in the part that
matters and changed in the part that does not:

- **Still borrowing.** `prompt` takes `&self`, so its future carries a
  `&AcpClient` and is not `'static`. `tokio::spawn` therefore still needs an
  `Arc<AcpClient>` — and `ControlledSession` owns its client by value, so
  reaching for one would change ownership across `acp_cli` well beyond this
  step. The boxed-future-in-the-driver conclusion stands, for the original
  reason.
- **No longer `!Unpin` by construction.** The old reason the box was
  *necessary* was that the future had to be pinned across iterations;
  `tokio::pin!` in a `loop` body cannot survive one. That is still true, so the
  box is still how it is done, but note what it buys: `Pin<Box<dyn Future>>` is
  `Unpin`, so `&mut F` is itself a `Future`, and dropping that borrow when the
  select loses the race does **not** drop the boxed future. That is what makes
  the arm cancel-safe, and it is a property of the box rather than of the
  session type — which is why the change of stack did not disturb it.
- **The output type changed and the cost did not.** `prompt` returns
  `anyhow::Result<PromptResponse>` rather than the engine's own result, so
  `report_turn` — which needs the whole response, not just the stop reason —
  runs in the arm, before the action is built. One `Box` allocation per turn,
  as before.

So `in_flight` stays, spelled the way `signals::Shutdown` already spells a
missing signal registration — and for the same reason, which is that neither
may `.unwrap()` an absent one:

```rust
/// Await the turn if there is one, and never resolve if there is not.
async fn in_flight<F: Future + Unpin>(slot: &mut Option<F>) -> F::Output {
    match slot {
        Some(turn) => turn.await,
        None => std::future::pending().await,
    }
}
```

**One correction to the sketch.** `Action::Permission` carries only the
`permission::Prompt`; the `PendingPermission` itself is `remember`ed in the
driver's map (§2.3), so the arm is a side effect plus a plain-data action
rather than a two-field one.

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

### 4.9 One loop, one exit — including `?`

`run` today has **four** ways out and only three of them reach
`ControlledSession::shutdown`: the two `break`s and the falling-off-the-end
path do, and every `?` — a failed paint, a failed redraw, a failed
`view.finish()` — does not. `Stdin`'s `Drop` still gives the terminal back
(I12 is unaffected), but the harness child is not confirmed reaped and the
controller credential is not revoked. The step-6 implementer left this here
deliberately, on the grounds that a single flat loop with one exit is the
natural fix rather than a fourth special case. It is.

So `run` splits in two:

- **`drive(...) -> Result<bool>`** holds the loop and every `?` in it, and
  returns whether the session ended abnormally.
- **`run(...)`** owns the view and stdin, calls `drive`, and then runs teardown
  **unconditionally** — `shutdown()`, then `view.finish()`, then the terminal,
  then the log tail — before deciding what to return.

A failed drive is an abnormal end, so it writes the session-log tail like any
other, and its error is the one reported. `View::open` and `Stdin::open` are the
only things left outside `drive`, and the one path where they fail shuts the
session down explicitly rather than by `?`. This is I10, and after this change
it holds for every exit rather than for most of them.

### 4.10 What can be trimmed in the same step (optional, separable)

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

Seven named regression tests, all of them on `step` with no runtime, no
terminal, and no session. They are the only coverage this surface has: `chat`'s
interactive loop has no integration test, and the three exits are checked by
the manual steps in `chat/mod.rs`.

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

And **T7 — every way out of `Answering` answers the question.** Not in the
original draft, because the shapes that reach it did not exist: flat, a turn can
*end* with a question still open (the agent gave up on its own tool call), and a
signal can arrive with one open (§6.2's second consequence). Both are new
reachable states, and both must resolve to the agent's reject option rather than
to silence. Written as an exhaustive sweep over `Action` from `Answering`, so a
variant added later fails the test rather than escaping it — this is I5, and it
is the invariant with the worst failure mode in the file.

---

## 6. Behaviour changes, and one open decision

### 6.1 `Ctrl-L` in a modal — **decided: (a), in its own commit**

Today it denies the permission / closes the picker, because both modals treat
any control chord as a cancel (§1.1). The flat table makes the cell visible.
Two options:

- **(a) Fix it.** `Ctrl-L` in `Answering` and `Routing` emits `Effect::Redraw`
  and stays. Consistent with every other phase; almost certainly what a user
  expects.
- **(b) Preserve it.** Keep the control-chord-is-cancel rule verbatim, and note
  it in the table as deliberate.

**Approved: (a), and landed as its own final commit.** The refactor implemented
**(b)** first — the table verbatim, both defective cells included — and T1
pinned it. The fix is a separate commit that changes two cells of that table
and nothing else, so it can be reverted without touching the machine.
Splitting it this way is the point: the flattening is behaviour-preserving and
reviewable as such, and the one deliberate key change is reviewable as
*itself*.

As implemented, `Ctrl-L` is checked **before** the control-chord-is-a-decline
rule in both modals, and the modal is put back rather than closed. It is the
only control chord exempted, and the reason is stated where the exemption is:
a redraw says nothing about the question being asked.

### 6.2 The transcript no longer freezes during a permission — intentional

§1.2's freeze goes away as a direct consequence of one select. This is the
change most visible to a user, and it is an improvement: the tool output that
*explains* what the agent is asking permission for now arrives while the
question is up, rather than after it is answered.

A second consequence rides with it, smaller and in the same direction: a
**signal** delivered while a permission is on screen is now acted on
immediately. Today `answer_permission` awaits stdin alone, so the `shutdown`
arm is not being polled at all and the signal waits for the question to be
answered before the session ends. Nothing is lost either way — `tokio`'s
handler keeps the signal pending — so this is latency, not correctness. It is
named because it is a difference a person can observe.

### 6.3 `/commands` stays a snapshot — **not adopted**

The original draft proposed making `/commands` live: as `Notice::Commands` the
view could re-render from the journal each frame, so an
`AvailableCommandsUpdate` arriving afterwards would be reflected. It is
strictly better and it does fall out of the enum.

**It is deliberately not done here.** This step is behaviour-preserving apart
from §6.1 and §6.2, both of which are named, tested, and — for §6.1 — isolated
in their own commit. A third change, in a surface with no integration coverage,
buys a refresh nobody has asked for at the cost of blurring what the refactor
did. `Notice::Commands` is still the effect's shape, because the reducer cannot
render; the driver answers it with the snapshot it renders today. Making it
live later is then a one-line change in the driver.

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

**As landed (2026-09-02).** 2A and 2B went together rather than as two steps.
The reason is the one §3.1 already gives from the other direction: a `Phase`
with `Idle` and `Routing` but no `Turn` has no reader for the turn, so 2A alone
would have shipped a machine that the driver consults for two of five loops and
bypasses for the other three — two sources of truth about what a key means,
which is the defect this whole spec exists to remove. `machine.rs` and the
flattening are therefore one commit; a reducer with no driver would have been
dead code in the same breath.

| commit | what it does | what a reviewer checks |
|---|---|---|
| 1 | `Prompt` gains `id` + `unanswered`; `Journal::pending_permission` deleted, `View::set_permission` added | that the journal is a projection of the update stream again, and nothing else writes it |
| 2 | `machine.rs`, and `run` flattened onto it: four phases, one select, `in_flight`, one exit through teardown | T1–T7, the picker table in §3.2 against the deleted `pick_route`, and the cancel-safety table in §4.7 |
| 3 | drop the permission pump task (§4.10) | that `StreamExt::next` on a retained stream is the same cancel-safety the channel gave |
| 4 | §6.1's two `Ctrl-L` cells | that it changes two cells of T1 and nothing else |

Commits 3 and 4 are separable on purpose: a bisect can put the flattening on
one side and either of them on the other.
