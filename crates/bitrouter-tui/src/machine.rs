//! What a chat session is doing, and therefore what a key means.
//!
//! # Why this exists
//!
//! The loop this replaced was five nested event loops — the session, the line
//! read, the turn, the picker, the permission — and **the nesting was the state
//! machine**. No value anywhere said "a turn is in flight" or "a modal owns the
//! keys"; those facts were encoded as which function happened to be on the
//! stack. So one key had four meanings, spread across [`crate::editor`] and two
//! hand-written control-chord branches in the application, and no single place
//! said so.
//!
//! [`Phase`] is that value, and [`step`] is the one place the table lives.
//!
//! # The reducer contract
//!
//! [`step`] is **synchronous, owns no I/O, holds no clock, and never touches
//! the journal.** Everything that awaits, paints, or answers a protocol request
//! is an [`Effect`] the caller runs. Two consequences are worth stating because
//! they are what make this testable at all:
//!
//! - **No `Instant`, not even for a tick.** [`crate::writer::Schedule`] already
//!   takes the clock as a parameter, so the reducer emits
//!   [`Effect::Paint`] with a [`Trigger`] and the driver decides whether that
//!   turns into a frame.
//! - **No resolver.** A permission is carried as a [`Prompt`] — plain data with
//!   an id — and answering one is [`Effect::Resolve`], which the driver turns
//!   into a lookup and a response. A reducer that could answer an agent would
//!   be doing I/O.
//!
//! # What the illegal states are
//!
//! [`Phase::Answering`] implies a turn; [`Phase::Routing`] implies no turn.
//! Both were reachability facts before and are now facts about the type, which
//! is the difference between "does not happen" and "cannot be written".
//!
//! # What is deliberately not here
//!
//! The turn future. [`State`] is plain data mutated through `&mut`, and a
//! future being polled across iterations cannot live there. It is the driver's,
//! and the invariant "there is a future exactly when the phase is `Turn` or
//! `Answering`" is maintained by [`Effect::Prompt`] setting it and
//! [`Action::TurnEnded`] and [`Effect::Cancel`] clearing it.

use std::collections::VecDeque;

use agent_client_protocol_schema::v1::{
    RequestPermissionOutcome, SelectedPermissionOutcome, StopReason,
};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::text::Line;

use crate::editor::{Edit, Editor};
use crate::permission::Prompt;
use crate::picker::Picker;
use crate::writer::Trigger;

/// What the session says when a turn is given up on.
const CANCELLED: &str = "[turn cancelled]";
/// What the session says when a question was answered by a choice.
const ANSWERED: &str = "permission answered";
/// What the session says when a question was answered by declining to choose.
const DENIED: &str = "permission denied";
/// What the session says when the agent asks with no turn to ask about.
const NO_TURN: &str = "permission denied: no turn is running";
/// What the session says when a picker was closed without choosing.
const ROUTE_UNCHANGED: &str = "route unchanged";
/// What the session says when the daemon suggested nothing to choose between.
const NO_ROUTES: &str = "no routes to choose between";
/// What the session says when `/route` is typed at a session that has no route
/// control to reach.
const NOT_ROUTABLE: &str = "this session cannot be rerouted (the controller advertises no route \
                            control: running direct, or without a trusted local daemon binding)";

/// What the session is doing, and therefore what a key means.
#[derive(Debug)]
pub enum Phase {
    /// No turn in flight. Keys go to the line editor.
    Idle,
    /// A turn is in flight and nothing is modal over it.
    Turn,
    /// A turn is in flight and a permission question owns the keys.
    Answering(Prompt),
    /// The route picker owns the keys. Reachable only from [`Phase::Idle`].
    Routing(Picker),
}

/// Everything the loop needs to remember between two events.
#[derive(Debug)]
pub struct State {
    /// What the session is doing.
    pub phase: Phase,
    /// The line being typed.
    pub editor: Editor,
    /// How many prompts this session has sent, so two in a row cannot merge
    /// into one run in the journal.
    pub prompts: usize,
    /// Whether the controller advertised both `_bitrouter/route/list` and
    /// `_bitrouter/route/set` for this session. A controller with no trusted
    /// local binding advertises neither, and `/route` then says so rather than
    /// opening a picker that cannot act.
    pub routable: bool,
    /// Questions the agent has asked and nobody has answered yet.
    ///
    /// A deque rather than a slot: the flat loop polls the permission stream
    /// while a question is already up, so a second one can arrive with the
    /// first still open. The nested loop serialised them by accident — nothing
    /// polled the stream — so making the queue explicit is what stops the
    /// second one silently replacing the first.
    pub queued: VecDeque<Prompt>,
}

impl State {
    /// A session at an idle prompt, having sent nothing.
    pub fn new(routable: bool) -> Self {
        Self {
            phase: Phase::Idle,
            editor: Editor::default(),
            prompts: 0,
            routable,
            queued: VecDeque::new(),
        }
    }

    /// Is a turn in flight? The driver's tick arm is gated on this: an idle
    /// session must not arm a 33 Hz timer for a transcript nothing is writing.
    pub fn streaming(&self) -> bool {
        matches!(self.phase, Phase::Turn | Phase::Answering(_))
    }
}

/// Something that happened, in the vocabulary the reducer understands.
#[derive(Debug)]
pub enum Action {
    /// One terminal event.
    Key(Event),
    /// stdin ended — the terminal went away.
    InputClosed,
    /// INT / TERM / HUP.
    Signal,
    /// The journal changed. A signal, not a payload: whoever applied the
    /// update has already applied it.
    Dirty,
    /// The streaming frame budget elapsed.
    Tick,
    /// The agent asked for permission.
    Permission(Prompt),
    /// `_bitrouter/route/list` came back, or failed with a rendered message.
    Routes(Result<Routes, String>),
    /// `_bitrouter/route/set` came back, carrying the route now actually in
    /// force — never the one that was asked for.
    Routed(Result<String, String>),
    /// The prompt turn settled, one way or the other.
    TurnEnded(Result<StopReason, String>),
}

/// What `_bitrouter/route/list` reported.
#[derive(Debug)]
pub struct Routes {
    /// The routes the daemon suggests.
    pub available: Vec<String>,
    /// The lease the daemon says is in force, if any.
    pub current: Option<String>,
}

/// Something the driver must do. Everything with a side effect is one of these.
#[derive(Debug)]
pub enum Effect {
    /// Consider a frame. Whether one is painted is the schedule's decision,
    /// and the schedule holds the clock.
    Paint(Trigger),
    /// Repaint everything, whatever the writer believes is on screen.
    Redraw,
    /// Echo the current line. Separate from [`Effect::Paint`] because the
    /// driver must push the buffer into the view before painting it.
    Echo,
    /// Say one thing, replacing whatever was said last.
    Notice(Notice),
    /// Stop saying anything.
    ClearNotice,
    /// Give a modal its row, or take the row back.
    Modal(Option<Line<'static>>),
    /// Show a permission question, or take it down once it is answered.
    ShowPermission(Option<Prompt>),
    /// Answer one permission request.
    Resolve {
        /// Which request, as [`Prompt::id`] names it.
        id: String,
        /// What to answer it with.
        outcome: RequestPermissionOutcome,
    },
    /// Send this line as a turn. `nth` keys the journal chunk, so two prompts
    /// in a row cannot merge into one paragraph.
    Prompt {
        /// The line to send.
        line: String,
        /// Which prompt of the session this is.
        nth: usize,
    },
    /// Give up on the turn in flight: tell the agent, and leave no question
    /// hanging.
    Cancel,
    /// Ask what routes are on offer.
    ListRoutes,
    /// Ask for this route to be installed.
    SetRoute(String),
    /// The route the footer names for the rest of the session — the one the
    /// daemon confirmed.
    RouteInForce(Option<String>),
    /// The session is over. Whoever runs this leaves through teardown.
    Exit,
}

/// What the session has to say.
#[derive(Debug)]
pub enum Notice {
    /// One line of the client's own text.
    Say(String),
    /// The agent's own command list, which only the driver can render — it is
    /// the journal that holds it, and the reducer never reads the journal.
    Commands,
}

/// Apply one action, and say what the driver must do about it.
pub fn step(state: &mut State, action: Action) -> Vec<Effect> {
    match action {
        // The hot action while streaming. An empty `Vec` does not allocate,
        // and the schedule is where a stream of these becomes one frame.
        Action::Dirty => vec![Effect::Paint(Trigger::Update)],
        Action::Tick => vec![Effect::Paint(Trigger::Tick)],
        Action::Signal => signal(state),
        Action::InputClosed => closed(state),
        Action::Key(event) => key(state, &event),
        Action::Permission(prompt) => permission(state, prompt),
        Action::Routes(listed) => routes(state, listed),
        Action::Routed(installed) => routed(installed),
        Action::TurnEnded(result) => turn_ended(state, result),
    }
}

/// Answer this request with the given outcome.
fn resolve(prompt: &Prompt, outcome: RequestPermissionOutcome) -> Effect {
    Effect::Resolve {
        id: prompt.id().to_string(),
        outcome,
    }
}

/// Leave whatever phase is current, answering every question it holds, and
/// return the phase that was left.
///
/// **This is I5.** Every path out of [`Phase::Answering`] that is not a choice
/// goes through here — a cancelled turn, the terminal ending, a signal, a turn
/// that settled with a question still open — so a question is never left for
/// whichever keystroke happens to arrive next. Queued questions go with it:
/// they are as unanswerable as the open one once nothing is left to ask on.
fn abandon(state: &mut State) -> (Phase, Vec<Effect>) {
    let phase = std::mem::replace(&mut state.phase, Phase::Idle);
    let mut effects = Vec::new();
    if let Phase::Answering(prompt) = &phase {
        effects.push(resolve(prompt, prompt.unanswered()));
    }
    for prompt in std::mem::take(&mut state.queued) {
        effects.push(resolve(&prompt, prompt.unanswered()));
    }
    if !effects.is_empty() {
        effects.push(Effect::ShowPermission(None));
    }
    (phase, effects)
}

/// Bring up the next queued question, or hand the keys back to the turn.
fn next_question(state: &mut State) -> Effect {
    match state.queued.pop_front() {
        Some(next) => {
            state.phase = Phase::Answering(next.clone());
            Effect::ShowPermission(Some(next))
        }
        None => {
            state.phase = Phase::Turn;
            Effect::ShowPermission(None)
        }
    }
}

/// What a settled turn leaves on screen.
fn settled(text: String) -> Vec<Effect> {
    vec![
        Effect::Notice(Notice::Say(text)),
        Effect::Echo,
        // A settled turn is immediate: it is the moment the reader is waiting
        // for, not streaming noise.
        Effect::Paint(Trigger::TurnSettled),
    ]
}

/// A signal leaves by the front door from every phase: the agent is shut down
/// and the terminal restored on the way out, and any open question is answered
/// first rather than left to teardown's ordering.
fn signal(state: &mut State) -> Vec<Effect> {
    let (_, mut effects) = abandon(state);
    effects.push(Effect::Exit);
    effects
}

/// The terminal went away. Nothing can answer the rest of this session.
fn closed(state: &mut State) -> Vec<Effect> {
    let (phase, mut effects) = abandon(state);
    match phase {
        Phase::Turn | Phase::Answering(_) => {
            effects.push(Effect::Cancel);
            effects.extend(settled(CANCELLED.to_string()));
        }
        Phase::Routing(_) => {
            effects.push(Effect::Modal(None));
            effects.push(Effect::Notice(Notice::Say(ROUTE_UNCHANGED.to_string())));
            effects.push(Effect::Paint(Trigger::Key));
        }
        Phase::Idle => {}
    }
    effects.push(Effect::Exit);
    effects
}

/// One key, read against the phase that decides what it means.
///
/// The phase is taken out and each branch puts back what it wants to be in;
/// a key that means nothing is the branch that puts back what it took.
fn key(state: &mut State, event: &Event) -> Vec<Effect> {
    match std::mem::replace(&mut state.phase, Phase::Idle) {
        Phase::Idle => idle_key(state, event),
        Phase::Turn => turn_key(state, event),
        Phase::Answering(prompt) => answering_key(state, prompt, event),
        Phase::Routing(picker) => routing_key(state, picker, event),
    }
}

/// At an idle prompt every key is the line editor's.
fn idle_key(state: &mut State, event: &Event) -> Vec<Effect> {
    match event {
        Event::Key(key) => match state.editor.apply(*key) {
            Edit::Ignored => Vec::new(),
            Edit::Changed => vec![Effect::Echo, Effect::Paint(Trigger::Key)],
            Edit::Redrawn => vec![Effect::Echo, Effect::Redraw],
            Edit::Submitted => submit(state),
            // Ctrl-C or Ctrl-D at an idle prompt. The session is over.
            Edit::Ended => vec![Effect::Exit],
        },
        // Bracketed paste arrives whole, which is the point: without it a
        // pasted line is indistinguishable from a fast typist and its newline
        // submits half of it.
        Event::Paste(text) => {
            state.editor.paste(text);
            vec![Effect::Echo, Effect::Paint(Trigger::Key)]
        }
        _ => Vec::new(),
    }
}

/// Enter at an idle prompt.
///
/// A blank submission is swallowed here rather than becoming a turn: an empty
/// prompt has nothing to ask.
fn submit(state: &mut State) -> Vec<Effect> {
    if state.editor.line().trim().is_empty() {
        state.editor.clear();
        return vec![Effect::Echo, Effect::Paint(Trigger::Key)];
    }
    let line = state.editor.take();
    // The last turn's word stands until this one starts, so a stop reason is
    // readable for as long as the reader is deciding what to say next.
    let mut effects = vec![Effect::Echo, Effect::ClearNotice];
    // A line of exactly `/commands` lists what the agent itself offers. Ours
    // are hardcoded; the agent's arrive on `AvailableCommandsUpdate`.
    if line.trim() == "/commands" {
        effects.push(Effect::Notice(Notice::Commands));
        effects.push(Effect::Paint(Trigger::Key));
        return effects;
    }
    // A line of exactly `/route` opens the picker — when there is a route
    // surface to open it against.
    if line.trim() == "/route" {
        if state.routable {
            effects.push(Effect::ListRoutes);
        } else {
            effects.push(Effect::Notice(Notice::Say(NOT_ROUTABLE.to_string())));
            effects.push(Effect::Paint(Trigger::Key));
        }
        return effects;
    }
    state.prompts = state.prompts.saturating_add(1);
    state.phase = Phase::Turn;
    effects.push(Effect::Prompt {
        line,
        nth: state.prompts,
    });
    effects.push(Effect::Paint(Trigger::Key));
    effects
}

/// During a turn only two keys mean anything, and neither of them types.
fn turn_key(state: &mut State, event: &Event) -> Vec<Effect> {
    // Ctrl-C and `Esc` during a turn are a cancel, not an exit: the session
    // survives it and the next prompt is drawn. The phase stays `Idle`, which
    // is where `abandon` left it.
    if crate::editor::is_cancel(event) {
        let mut effects = vec![Effect::Cancel];
        effects.extend(settled(CANCELLED.to_string()));
        return effects;
    }
    state.phase = Phase::Turn;
    if crate::editor::is_redraw(event) {
        return vec![Effect::Redraw];
    }
    Vec::new()
}

/// With a question up, the keys are the question's.
fn answering_key(state: &mut State, prompt: Prompt, event: &Event) -> Vec<Effect> {
    let Some(key) = crate::editor::press(event) else {
        state.phase = Phase::Answering(prompt);
        return Vec::new();
    };
    // A control chord is never a choice: Ctrl-C must answer the question the
    // only way an interrupt can be read — no — rather than selecting whatever
    // `c` happens to be numbered.
    let declined = key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Esc;
    let answer = if declined {
        Some((prompt.unanswered(), DENIED))
    } else if let KeyCode::Char(c) = key.code {
        prompt.choose(c).map(|id| {
            (
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                ANSWERED,
            )
        })
    } else {
        None
    };
    // An unrecognised key selects nothing and the prompt stays up. A prompt
    // that treated a stray keystroke as consent would be worse than no prompt.
    let Some((outcome, said)) = answer else {
        state.phase = Phase::Answering(prompt);
        return Vec::new();
    };
    // Answering does **not** cancel the turn: the agent asked mid-turn and
    // carries on with the answer.
    vec![
        resolve(&prompt, outcome),
        next_question(state),
        Effect::Notice(Notice::Say(said.to_string())),
        Effect::Paint(Trigger::Permission),
    ]
}

/// With the picker up, the keys are the picker's.
fn routing_key(state: &mut State, picker: Picker, event: &Event) -> Vec<Effect> {
    let Some(key) = crate::editor::press(event) else {
        state.phase = Phase::Routing(picker);
        return Vec::new();
    };
    // A control chord is never a choice — Ctrl-C closes the picker instead of
    // selecting whatever `c` happens to be.
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Esc {
        return vec![
            Effect::Modal(None),
            Effect::Notice(Notice::Say(ROUTE_UNCHANGED.to_string())),
            Effect::Paint(Trigger::Key),
        ];
    }
    if let KeyCode::Char(c) = key.code
        && let Some(route) = picker.choose(c)
    {
        // A route to *attempt*. Only what the daemon confirms is in force.
        return vec![Effect::Modal(None), Effect::SetRoute(route)];
    }
    state.phase = Phase::Routing(picker);
    Vec::new()
}

/// The agent asked for permission.
fn permission(state: &mut State, prompt: Prompt) -> Vec<Effect> {
    match std::mem::replace(&mut state.phase, Phase::Idle) {
        Phase::Turn => {
            state.phase = Phase::Answering(prompt.clone());
            vec![
                Effect::ShowPermission(Some(prompt)),
                Effect::Paint(Trigger::Permission),
            ]
        }
        // A second question with the first still open queues rather than
        // replacing it — silently losing one would leave the agent parked.
        open @ Phase::Answering(_) => {
            state.phase = open;
            state.queued.push_back(prompt);
            Vec::new()
        }
        // No turn to ask about. This is the state a permission that outlived
        // its turn lands in, and denying it here is what stops it being drawn
        // during the *next* turn as though that turn had asked.
        quiet => {
            state.phase = quiet;
            vec![
                resolve(&prompt, prompt.unanswered()),
                Effect::Notice(Notice::Say(NO_TURN.to_string())),
                Effect::Paint(Trigger::Permission),
            ]
        }
    }
}

/// `_bitrouter/route/list` came back.
fn routes(state: &mut State, listed: Result<Routes, String>) -> Vec<Effect> {
    // Only an idle session asked for this, and only an idle session can open a
    // picker over the keys.
    if !matches!(state.phase, Phase::Idle) {
        return Vec::new();
    }
    let listed = match listed {
        Ok(listed) => listed,
        Err(error) => {
            return vec![
                Effect::Notice(Notice::Say(format!("route unchanged: {error}"))),
                Effect::Paint(Trigger::Key),
            ];
        }
    };
    // `routable` is the gate, asked again here so there is no way to draw a
    // picker without answering it.
    let Some(picker) = Picker::open(state.routable, &listed.available, listed.current.as_deref())
    else {
        return vec![
            Effect::Notice(Notice::Say(NO_ROUTES.to_string())),
            Effect::Paint(Trigger::Key),
        ];
    };
    let row = picker.render();
    state.phase = Phase::Routing(picker);
    vec![Effect::Modal(Some(row)), Effect::Paint(Trigger::Key)]
}

/// `_bitrouter/route/set` came back. What it *confirmed* is what is reported,
/// never what was asked for: `set` can legitimately refuse.
fn routed(installed: Result<String, String>) -> Vec<Effect> {
    match installed {
        Ok(in_force) => vec![
            // The footer names the route for the rest of the session, not just
            // for this frame.
            Effect::RouteInForce(Some(in_force.clone())),
            Effect::Notice(Notice::Say(format!("route: {in_force}"))),
            Effect::Paint(Trigger::Key),
        ],
        Err(message) => vec![
            Effect::Notice(Notice::Say(message)),
            Effect::Paint(Trigger::Key),
        ],
    }
}

/// The turn settled.
fn turn_ended(state: &mut State, result: Result<StopReason, String>) -> Vec<Effect> {
    // A turn can settle with a question still open — the agent gave up on its
    // own tool call — and that question has to be answered, not forgotten.
    let (phase, mut effects) = abandon(state);
    match phase {
        Phase::Turn | Phase::Answering(_) => effects.extend(settled(match result {
            Ok(stop) => format!("[{stop:?}]"),
            Err(error) => format!("turn failed: {error}"),
        })),
        // Unreachable: the driver clears the turn future on every path that
        // leaves `Turn`. A reducer is total, so it says so rather than
        // asserting it.
        quiet => state.phase = quiet,
    }
    effects
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{PermissionOption, PermissionOptionId};
    use agent_client_protocol_schema::v1::{PermissionOptionKind, RequestPermissionOutcome};
    use crossterm::event::{KeyEvent, KeyEventKind};

    use super::*;

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), id, kind)
    }

    /// A question offering allow / always / reject, in that order.
    fn question(id: &str) -> Prompt {
        Prompt::new(
            id,
            Some("Write src/main.rs".to_string()),
            "t1",
            vec![
                option("allow", PermissionOptionKind::AllowOnce),
                option("always", PermissionOptionKind::AllowAlways),
                option("no", PermissionOptionKind::RejectOnce),
            ],
        )
    }

    fn picker() -> Picker {
        Picker::open(
            true,
            &["@balanced".to_string(), "openai:gpt-5".to_string()],
            Some("@balanced"),
        )
        .expect("a picker with two routes to choose between")
    }

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    /// The name a phase goes by in an assertion.
    fn phase_of(state: &State) -> &'static str {
        match state.phase {
            Phase::Idle => "idle",
            Phase::Turn => "turn",
            Phase::Answering(_) => "answering",
            Phase::Routing(_) => "routing",
        }
    }

    /// The names the effects go by in an assertion. Deliberately lossy: the
    /// key table is about *which* effects a key produces and in what order,
    /// and the payloads are asserted by the tests that are about payloads.
    fn effects_of(effects: &[Effect]) -> Vec<&'static str> {
        effects
            .iter()
            .map(|effect| match effect {
                Effect::Paint(_) => "paint",
                Effect::Redraw => "redraw",
                Effect::Echo => "echo",
                Effect::Notice(_) => "notice",
                Effect::ClearNotice => "clear-notice",
                Effect::Modal(_) => "modal",
                Effect::ShowPermission(_) => "show-permission",
                Effect::Resolve { .. } => "resolve",
                Effect::Prompt { .. } => "prompt",
                Effect::Cancel => "cancel",
                Effect::ListRoutes => "list-routes",
                Effect::SetRoute(_) => "set-route",
                Effect::RouteInForce(_) => "route-in-force",
                Effect::Exit => "exit",
            })
            .collect()
    }

    /// The option the **first** effect selected for `id`, if the first effect
    /// is an answer to `id` and it selected one at all.
    ///
    /// Deliberately first-only: I5 is about a question being answered *before*
    /// anything else happens to it, not merely somewhere in the list.
    fn answered_with(effects: &[Effect], id: &str) -> Option<String> {
        match effects.first() {
            Some(Effect::Resolve {
                id: answered,
                outcome: RequestPermissionOutcome::Selected(selected),
            }) if answered == id => Some(selected.option_id.0.to_string()),
            _ => None,
        }
    }

    /// A session in each phase, built the same way every test builds one.
    fn in_phase(phase: &str) -> State {
        let mut state = State::new(true);
        state.phase = match phase {
            "turn" => Phase::Turn,
            "answering" => Phase::Answering(question("r1")),
            "routing" => Phase::Routing(picker()),
            // `"idle"`, and anything a test misspells — which the phase
            // assertion that follows every use of this will catch.
            _ => Phase::Idle,
        };
        state
    }

    /// **T1 — the key table.**
    ///
    /// One table over (phase × key), covering every cell of the table in
    /// `CHAT_MACHINE_SPEC.md` §1.1. This is the artifact the whole refactor is
    /// for: before it, Ctrl-C's four context-dependent meanings lived in
    /// `editor::apply`, `editor::is_cancel`, and two hand-written control-chord
    /// branches in the application, and nothing put them next to each other.
    ///
    /// The `Ctrl-L` cells in `answering` and `routing` say **deny** and
    /// **close**, which is a defect — both modals read any control chord as a
    /// cancel, so asking for a redraw answers the agent's question. It is
    /// pinned here as it is, deliberately: the flattening preserves behaviour,
    /// and the fix is its own change.
    #[test]
    fn the_key_table_is_one_table() {
        // (phase, key, what it does, the phase it leaves behind)
        let table: &[(&str, Event, &[&str], &str)] = &[
            // Ctrl-C: exit / cancel the turn / deny / close.
            ("idle", ctrl('c'), &["exit"], "idle"),
            (
                "turn",
                ctrl('c'),
                &["cancel", "notice", "echo", "paint"],
                "idle",
            ),
            (
                "answering",
                ctrl('c'),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            ("routing", ctrl('c'), &["modal", "notice", "paint"], "idle"),
            // Esc: ignored / cancel the turn / deny / close.
            ("idle", press(KeyCode::Esc), &[], "idle"),
            (
                "turn",
                press(KeyCode::Esc),
                &["cancel", "notice", "echo", "paint"],
                "idle",
            ),
            (
                "answering",
                press(KeyCode::Esc),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            (
                "routing",
                press(KeyCode::Esc),
                &["modal", "notice", "paint"],
                "idle",
            ),
            // Ctrl-D: exit / ignored / deny / close.
            ("idle", ctrl('d'), &["exit"], "idle"),
            ("turn", ctrl('d'), &[], "turn"),
            (
                "answering",
                ctrl('d'),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            ("routing", ctrl('d'), &["modal", "notice", "paint"], "idle"),
            // Ctrl-L: redraw / redraw / **deny** / **close**. The last two
            // are the defect this table makes visible.
            ("idle", ctrl('l'), &["echo", "redraw"], "idle"),
            ("turn", ctrl('l'), &["redraw"], "turn"),
            (
                "answering",
                ctrl('l'),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            ("routing", ctrl('l'), &["modal", "notice", "paint"], "idle"),
            // Ctrl-W: delete a word / ignored / deny / close.
            ("idle", ctrl('w'), &["echo", "paint"], "idle"),
            ("turn", ctrl('w'), &[], "turn"),
            (
                "answering",
                ctrl('w'),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            ("routing", ctrl('w'), &["modal", "notice", "paint"], "idle"),
            // A digit: types it / ignored / selects an option / selects a
            // route.
            (
                "idle",
                press(KeyCode::Char('1')),
                &["echo", "paint"],
                "idle",
            ),
            ("turn", press(KeyCode::Char('1')), &[], "turn"),
            (
                "answering",
                press(KeyCode::Char('1')),
                &["resolve", "show-permission", "notice", "paint"],
                "turn",
            ),
            (
                "routing",
                press(KeyCode::Char('1')),
                &["modal", "set-route"],
                "idle",
            ),
            // Enter: submits / ignored / nothing / nothing. An empty line at
            // an idle prompt is swallowed, which is why this row echoes rather
            // than prompting.
            ("idle", press(KeyCode::Enter), &["echo", "paint"], "idle"),
            ("turn", press(KeyCode::Enter), &[], "turn"),
            ("answering", press(KeyCode::Enter), &[], "answering"),
            ("routing", press(KeyCode::Enter), &[], "routing"),
            // Any other printable key: types it / ignored / nothing / nothing.
            (
                "idle",
                press(KeyCode::Char('x')),
                &["echo", "paint"],
                "idle",
            ),
            ("turn", press(KeyCode::Char('x')), &[], "turn"),
            ("answering", press(KeyCode::Char('x')), &[], "answering"),
            ("routing", press(KeyCode::Char('x')), &[], "routing"),
        ];

        for (phase, event, expected, after) in table {
            let mut state = in_phase(phase);
            let effects = step(&mut state, Action::Key(event.clone()));
            assert_eq!(
                effects_of(&effects),
                *expected,
                "{phase} + {event:?} did the wrong thing"
            );
            assert_eq!(
                phase_of(&state),
                *after,
                "{phase} + {event:?} left the wrong phase"
            );
        }
    }

    /// A key *release* must not double the keystroke, in any phase. The
    /// editor already refuses one; the two modals have to refuse it too, and
    /// the reason to check them here is that they read the raw `KeyEvent`
    /// rather than going through the editor.
    #[test]
    fn a_key_release_means_nothing_anywhere() {
        for phase in ["idle", "turn", "answering", "routing"] {
            let mut release = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
            release.kind = KeyEventKind::Release;
            let mut state = in_phase(phase);
            let effects = step(&mut state, Action::Key(Event::Key(release)));
            assert!(
                effects.is_empty(),
                "a key release did something in {phase}: {effects:?}"
            );
            assert_eq!(phase_of(&state), phase, "and it changed the phase");
        }
    }

    /// **T2 — cancelling with a question outstanding denies it.**
    ///
    /// The `Resolve` must come **first** and must carry the agent's own reject
    /// option, never a selection. Cancelling is not consenting.
    #[test]
    fn every_way_out_of_a_question_denies_it_first() {
        for (name, action) in [
            ("the terminal ending", Action::InputClosed),
            ("a signal", Action::Signal),
            ("a control chord", Action::Key(ctrl('c'))),
            ("escape", Action::Key(press(KeyCode::Esc))),
            (
                "the turn settling under it",
                Action::TurnEnded(Ok(StopReason::EndTurn)),
            ),
        ] {
            let mut state = in_phase("answering");
            let effects = step(&mut state, action);
            assert_eq!(
                answered_with(&effects, "r1").as_deref(),
                Some("no"),
                "{name} must answer the question first, with the agent's own \
                 reject option: {effects:?}"
            );
        }
    }

    /// **T3 — Ctrl-C during a permission does not cancel the turn.**
    ///
    /// Easy to lose while flattening: the nested loop consumed the keystroke
    /// inside `answer_permission`, so the turn loop's cancel flag never saw
    /// it. One careless fallthrough turns "deny this" into "abandon the turn".
    #[test]
    fn declining_a_question_leaves_the_turn_running() {
        for event in [ctrl('c'), press(KeyCode::Esc), ctrl('l')] {
            let mut state = in_phase("answering");
            let effects = step(&mut state, Action::Key(event.clone()));
            assert_eq!(
                phase_of(&state),
                "turn",
                "{event:?} ended the turn instead of the question"
            );
            assert!(
                !effects_of(&effects).contains(&"cancel"),
                "{event:?} cancelled the turn: {effects:?}"
            );
        }
    }

    /// **T4 — a second question queues rather than replaces.**
    ///
    /// Not writable against the old shape at all: nothing polled the
    /// permission stream while a question was up, so the transport serialised
    /// them by accident.
    #[test]
    fn a_second_question_waits_its_turn() {
        let mut state = in_phase("turn");
        assert_eq!(
            effects_of(&step(&mut state, Action::Permission(question("r1")))),
            ["show-permission", "paint"]
        );
        // The second arrives with the first still open, and is silent: the
        // first is still the one on screen.
        assert!(
            step(&mut state, Action::Permission(question("r2"))).is_empty(),
            "the second question must not repaint over the first"
        );
        assert_eq!(phase_of(&state), "answering");

        // Answering the first brings the second up by itself.
        let effects = step(&mut state, Action::Key(press(KeyCode::Char('1'))));
        assert_eq!(
            effects_of(&effects),
            ["resolve", "show-permission", "notice", "paint"]
        );
        let shown = effects.iter().find_map(|effect| match effect {
            Effect::ShowPermission(Some(prompt)) => Some(prompt.id().to_string()),
            _ => None,
        });
        assert_eq!(
            shown.as_deref(),
            Some("r2"),
            "the queued question must come up on its own"
        );
        assert_eq!(phase_of(&state), "answering");

        // And answering that one hands the keys back to the turn.
        let _ = step(&mut state, Action::Key(press(KeyCode::Char('1'))));
        assert_eq!(phase_of(&state), "turn");
    }

    /// **T5 — the transcript keeps streaming while a question is up.**
    ///
    /// The one intentional behaviour change the flattening makes on its own:
    /// the old `answer_permission` awaited stdin and nothing else, so the
    /// frame that would have explained *what* the agent is asking about did
    /// not arrive until after the answer. `Dirty` and `Tick` must be live in
    /// every phase that has a turn behind it.
    #[test]
    fn a_question_does_not_freeze_the_transcript() {
        for phase in ["turn", "answering"] {
            let mut state = in_phase(phase);
            assert!(state.streaming(), "{phase} must arm the frame budget");
            assert_eq!(
                effects_of(&step(&mut state, Action::Dirty)),
                ["paint"],
                "an update in {phase} must still reach the schedule"
            );
            assert_eq!(effects_of(&step(&mut state, Action::Tick)), ["paint"]);
            assert_eq!(phase_of(&state), phase, "and neither may change the phase");
        }
        // An idle session arms no timer at all: one ticking 33 times a second
        // at an empty prompt is a wake-up for nothing.
        assert!(!in_phase("idle").streaming());
        assert!(!in_phase("routing").streaming());
    }

    /// **T6 — a permission outside a turn is denied.**
    ///
    /// A question the agent emits after its turn has settled used to sit in
    /// the transport until the *next* turn's select drew it out, where it was
    /// painted as though the current tool call had asked for it. Flat, it
    /// arrives at idle and is denied there.
    #[test]
    fn a_question_with_no_turn_behind_it_is_denied() {
        for phase in ["idle", "routing"] {
            let mut state = in_phase(phase);
            let effects = step(&mut state, Action::Permission(question("late")));
            assert_eq!(effects_of(&effects), ["resolve", "notice", "paint"]);
            assert_eq!(
                answered_with(&effects, "late").as_deref(),
                Some("no"),
                "a question nobody can answer must not become an allow"
            );
            assert_eq!(
                phase_of(&state),
                phase,
                "and it must not take the keys from {phase}"
            );
        }
    }

    /// **T7 — every way out of `Answering` answers the question.**
    ///
    /// A sweep over the whole `Action` vocabulary rather than the paths that
    /// happen to be reachable today, because two of them only became
    /// reachable when the loop went flat: a turn can now settle with a
    /// question open, and a signal is now observed while one is up. The rule
    /// is the one with the worst failure mode in the file — a question left
    /// unanswered is a harness parked forever, and a question answered by the
    /// wrong rule is consent nobody gave.
    #[test]
    fn leaving_a_question_never_leaves_it_unanswered() {
        let sweep = || {
            vec![
                Action::Key(ctrl('c')),
                Action::Key(press(KeyCode::Esc)),
                Action::Key(ctrl('d')),
                Action::Key(ctrl('l')),
                Action::Key(ctrl('w')),
                Action::Key(press(KeyCode::Char('1'))),
                Action::Key(press(KeyCode::Char('x'))),
                Action::Key(press(KeyCode::Enter)),
                Action::Key(Event::Paste("pasted".to_string())),
                Action::InputClosed,
                Action::Signal,
                Action::Dirty,
                Action::Tick,
                Action::Permission(question("second")),
                Action::Routes(Ok(Routes {
                    available: vec!["@balanced".to_string()],
                    current: None,
                })),
                Action::Routed(Ok("@balanced".to_string())),
                Action::TurnEnded(Ok(StopReason::EndTurn)),
                Action::TurnEnded(Err("the harness died".to_string())),
            ]
        };
        for action in sweep() {
            let mut state = in_phase("answering");
            let name = format!("{action:?}");
            let effects = step(&mut state, action);
            let answered = effects
                .iter()
                .any(|effect| matches!(effect, Effect::Resolve { id, .. } if id == "r1"));
            // Either the question is still up — nobody left it — or it was
            // answered on the way out. There is no third outcome.
            let still_up = matches!(&state.phase, Phase::Answering(open) if open.id() == "r1");
            assert!(
                answered ^ still_up,
                "{name}: answered={answered}, still up={still_up}"
            );
        }
    }

    /// A blank line is swallowed; a real one becomes a turn, numbered, with
    /// the notice cleared out of the way first.
    #[test]
    fn a_submitted_line_becomes_a_numbered_turn() {
        let mut state = State::new(true);
        // Enter on nothing types nothing and sends nothing.
        assert!(matches!(state.phase, Phase::Idle));
        let _ = step(&mut state, Action::Key(press(KeyCode::Enter)));
        assert_eq!(state.prompts, 0);

        for (typed, expected) in [("hello", 1), ("again", 2)] {
            for c in typed.chars() {
                let _ = step(&mut state, Action::Key(press(KeyCode::Char(c))));
            }
            let effects = step(&mut state, Action::Key(press(KeyCode::Enter)));
            assert_eq!(
                effects_of(&effects),
                ["echo", "clear-notice", "prompt", "paint"]
            );
            let sent = effects.iter().find_map(|effect| match effect {
                Effect::Prompt { line, nth } => Some((line.clone(), *nth)),
                _ => None,
            });
            assert_eq!(sent, Some((typed.to_string(), expected)));
            assert_eq!(phase_of(&state), "turn");
            assert_eq!(state.editor.line(), "", "the line goes with the prompt");
            // Back to idle for the next one.
            let _ = step(&mut state, Action::TurnEnded(Ok(StopReason::EndTurn)));
            assert_eq!(phase_of(&state), "idle");
        }
    }

    /// `/route` is gated on the capability the controller advertised, and the
    /// answer when it is absent is a sentence rather than a dead picker.
    #[test]
    fn route_needs_the_capability_the_controller_advertised() {
        for (routable, expected) in [(true, "list-routes"), (false, "notice")] {
            let mut state = State::new(routable);
            for c in "/route".chars() {
                let _ = step(&mut state, Action::Key(press(KeyCode::Char(c))));
            }
            let effects = step(&mut state, Action::Key(press(KeyCode::Enter)));
            assert_eq!(effects_of(&effects)[2], expected);
            assert_eq!(phase_of(&state), "idle", "no picker either way, yet");
        }
    }

    /// The picker's whole round trip: what it opens over, what it refuses to
    /// open over, and that a selection is an *attempt* rather than a change.
    #[test]
    fn the_picker_opens_chooses_and_reports_what_was_confirmed() {
        let mut state = State::new(true);
        let effects = step(
            &mut state,
            Action::Routes(Ok(Routes {
                available: vec!["@balanced".to_string(), "openai:gpt-5".to_string()],
                current: Some("@balanced".to_string()),
            })),
        );
        assert_eq!(effects_of(&effects), ["modal", "paint"]);
        assert_eq!(phase_of(&state), "routing");

        let effects = step(&mut state, Action::Key(press(KeyCode::Char('2'))));
        assert_eq!(effects_of(&effects), ["modal", "set-route"]);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SetRoute(route) if route == "openai:gpt-5"))
        );
        assert_eq!(phase_of(&state), "idle");

        // What the daemon confirmed is what the footer names — which need not
        // be what was asked for.
        let effects = step(&mut state, Action::Routed(Ok("@balanced".to_string())));
        assert_eq!(effects_of(&effects), ["route-in-force", "notice", "paint"]);
        assert!(effects.iter().any(
            |effect| matches!(effect, Effect::RouteInForce(Some(route)) if route == "@balanced")
        ));

        // A refusal changes nothing but what is said.
        let effects = step(
            &mut state,
            Action::Routed(Err("route unchanged: no".to_string())),
        );
        assert_eq!(effects_of(&effects), ["notice", "paint"]);
    }

    /// Nothing to choose between is no picker at all — not an empty one, and
    /// not one drawn against a capability that is absent.
    #[test]
    fn no_routes_and_no_capability_both_mean_no_picker() {
        for (routable, available) in [(true, Vec::new()), (false, vec!["@balanced".to_string()])] {
            let mut state = State::new(routable);
            let effects = step(
                &mut state,
                Action::Routes(Ok(Routes {
                    available,
                    current: None,
                })),
            );
            assert_eq!(effects_of(&effects), ["notice", "paint"]);
            assert_eq!(phase_of(&state), "idle");
        }

        // And a list that never came back says why, without a picker.
        let mut state = State::new(true);
        let effects = step(&mut state, Action::Routes(Err("the daemon is gone".into())));
        assert_eq!(effects_of(&effects), ["notice", "paint"]);
        assert_eq!(phase_of(&state), "idle");
    }

    /// Every phase leaves by the same door, and the door is always the last
    /// thing it does — teardown is the driver's, and it runs after the loop.
    #[test]
    fn every_phase_exits_through_the_same_effect() {
        for phase in ["idle", "turn", "answering", "routing"] {
            for action in [Action::Signal, Action::InputClosed] {
                let mut state = in_phase(phase);
                let name = format!("{phase} + {action:?}");
                let effects = step(&mut state, action);
                assert!(
                    matches!(effects.last(), Some(Effect::Exit)),
                    "{name} did not end the session: {effects:?}"
                );
                assert_eq!(
                    effects
                        .iter()
                        .filter(|effect| matches!(effect, Effect::Exit))
                        .count(),
                    1,
                    "{name} asked to exit more than once"
                );
            }
        }
    }

    /// The terminal going away mid-turn tells the agent, rather than only
    /// dropping the future and leaving it working.
    #[test]
    fn the_terminal_ending_mid_turn_cancels_before_it_exits() {
        let mut state = in_phase("turn");
        assert_eq!(
            effects_of(&step(&mut state, Action::InputClosed)),
            ["cancel", "notice", "echo", "paint", "exit"]
        );
    }

    /// A turn that failed says so, and a turn that ended says what ended it.
    #[test]
    fn a_settled_turn_reports_what_settled_it() {
        for (result, expected) in [
            (Ok(StopReason::MaxTokens), "[MaxTokens]"),
            (
                Err("the harness died".to_string()),
                "turn failed: the harness died",
            ),
        ] {
            let mut state = in_phase("turn");
            let effects = step(&mut state, Action::TurnEnded(result));
            assert_eq!(effects_of(&effects), ["notice", "echo", "paint"]);
            let said = effects.iter().find_map(|effect| match effect {
                Effect::Notice(Notice::Say(text)) => Some(text.clone()),
                _ => None,
            });
            assert_eq!(said.as_deref(), Some(expected));
            assert_eq!(phase_of(&state), "idle");
        }
    }
}
