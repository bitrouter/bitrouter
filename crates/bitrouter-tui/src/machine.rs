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
use crate::permission::{Decision, Policy, Prompt};
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
/// What the session says when a route lease is dropped and the daemon's own
/// default takes over again.
const ROUTE_RESET: &str = "route reset to the daemon's default";

/// One of BitRouter's own slash commands, as the reducer needs to know it.
///
/// Built by the app from the `ACTIONS` rows that carry a `tui_command`; the
/// reducer never sees the table, so it cannot offer a command the table does
/// not have. That is the whole of the coupling: this crate depends on nothing
/// of BitRouter's, so the set of commands is data handed in, exactly as
/// `routable` was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The words after the slash, space-separated: `"status"`, `"route reset"`.
    pub name: &'static str,
    /// The `ACTIONS` row id, matched by the reducer only for the ids in
    /// [`REDUCER_OWNED`].
    pub action: &'static str,
    /// One line for `/commands`.
    pub summary: &'static str,
    /// `Some(reason)` when the row's requirement is unmet in this session. The
    /// command is still listed — with the reason — and typing it answers with
    /// the reason instead of running. Absent, never dead.
    pub unavailable: Option<&'static str>,
}

/// A user-authored command that expands to a prompt.
///
/// Plain data: the reducer substitutes and sends. It never runs anything, which
/// is why this registry can be open — the config is the user's, unreviewed —
/// while the commands that reach BitRouter's own ports stay a closed, guarded
/// table.
///
/// Held in a **separate field** from [`Command`], never merged into one map.
/// The two are checked against each other once, when the config loads, so no
/// runtime precedence rule between them is ever needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCommand {
    /// The word after the slash.
    pub name: String,
    /// One line for `/commands`.
    pub description: String,
    /// `$ARGUMENTS` is replaced by everything typed after the name.
    pub template: String,
}

/// The rows the reducer dispatches itself rather than handing to the app,
/// because each needs reducer-owned state — the journal's command list, or the
/// picker's phase.
///
/// A guard asserts every entry is a row id that carries a `tui_command`.
pub const REDUCER_OWNED: &[&str] = &["commands", "route_set", "route_reset"];

/// Names that mean another command. A guard asserts no alias is itself a
/// `tui_command`, so an alias can never shadow a real name.
pub const ALIASES: &[(&str, &str)] = &[("help", "commands")];

/// What a submitted line turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A BitRouter command the reducer runs itself.
    Owned {
        /// The `ACTIONS` row id.
        action: &'static str,
        /// The rest of the line, whitespace-split — never one string.
        args: Vec<String>,
    },
    /// A BitRouter command the app answers through its action ports.
    Action {
        /// The `ACTIONS` row id, handed back to the app unchanged.
        action: &'static str,
        /// The rest of the line, whitespace-split — never one string.
        args: Vec<String>,
    },
    /// A listed BitRouter command this session cannot run, and why.
    Unavailable(&'static str),
    /// A prompt-expansion command, already expanded. Sent as a turn.
    Expand(String),
    /// Not ours — a prompt, including any command the agent advertises.
    Prompt(String),
}

/// Resolve one submitted line against the commands this session offers.
///
/// Precedence is fixed here and nowhere else: a two-word BitRouter name, then a
/// one-word one, then an alias, then the agent. Local wins, which is why an
/// agent that advertises `/status` is shadowed rather than obeyed — `/commands`
/// says so rather than the shadowing being silent.
///
/// A free function over a slice, not a method, so the piped loop and the
/// headless one-shot path can run the same resolution the terminal does.
pub fn resolve(commands: &[Command], prompt_commands: &[PromptCommand], line: &str) -> Resolution {
    let line = line.trim();
    let Some(rest) = line.strip_prefix('/') else {
        return Resolution::Prompt(line.to_string());
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    // Longest name first, so `/route reset` is not `/route` with an argument.
    for take in [2, 1] {
        let Some(head) = words.get(..take) else {
            continue;
        };
        let typed = head.join(" ");
        let name = ALIASES
            .iter()
            .find(|(alias, _)| *alias == typed)
            .map_or(typed.as_str(), |(_, target)| *target);
        if let Some(command) = commands.iter().find(|candidate| candidate.name == name) {
            if let Some(reason) = command.unavailable {
                return Resolution::Unavailable(reason);
            }
            let args = words[take..].iter().map(|word| word.to_string()).collect();
            // The reducer knows three verbs by name; everything else is an
            // opaque id the app resolves against the same ports the CLI uses.
            return if REDUCER_OWNED.contains(&command.action) {
                Resolution::Owned {
                    action: command.action,
                    args,
                }
            } else {
                Resolution::Action {
                    action: command.action,
                    args,
                }
            };
        }
    }
    // Only after every BitRouter name has missed. A config command can never
    // reach here under a name BitRouter answers — that config is refused at
    // load — so this order is a statement of precedence, not a tiebreak.
    if let Some(expansion) = words
        .first()
        .and_then(|name| prompt_commands.iter().find(|command| command.name == *name))
    {
        return Resolution::Expand(
            expansion
                .template
                .replace("$ARGUMENTS", &words[1..].join(" ")),
        );
    }
    Resolution::Prompt(line.to_string())
}

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
    /// The user's prompt-expansion commands.
    pub prompt_commands: Vec<PromptCommand>,
    /// BitRouter's own commands, in `/commands` order.
    ///
    /// Replaces the old `routable` flag: whether `/route` can act is now one
    /// entry's `unavailable`, which generalises to every command without the
    /// reducer growing a flag per capability.
    pub commands: Vec<Command>,
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
    pub fn new(commands: Vec<Command>) -> Self {
        Self {
            phase: Phase::Idle,
            editor: Editor::default(),
            prompts: 0,
            commands,
            prompt_commands: Vec::new(),
            queued: VecDeque::new(),
        }
    }

    /// Is this row offered *and* runnable here?
    ///
    /// Replaces the two reads of `routable`. A command that is listed with a
    /// reason is offered but not runnable, so this is the question every gate
    /// asks.
    pub fn available(&self, action: &str) -> bool {
        self.commands
            .iter()
            .any(|command| command.action == action && command.unavailable.is_none())
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
    /// `_bitrouter/route/set` or `/reset` came back, carrying the route now
    /// actually in force — never the one that was asked for. `Ok(None)` is the
    /// lease being gone, so the daemon's own default applies.
    Routed(Result<Option<String>, String>),
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
    /// Drop this session's route lease, so the daemon's default applies again.
    ResetRoute,
    /// Run this `ACTIONS` row through the app's ports and show what it reports
    /// as a notice. Emitted for every BitRouter command the reducer does not
    /// own itself.
    Action {
        /// The row id.
        action: &'static str,
        /// The rest of the line, already split.
        args: Vec<String>,
    },
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
fn answer_with(prompt: &Prompt, outcome: RequestPermissionOutcome) -> Effect {
    Effect::Resolve {
        id: prompt.id().to_string(),
        outcome,
    }
}

/// Answer a question the way a headless policy says to.
///
/// The pipe's counterpart of `answering_key`: a person's keystroke and a
/// policy's rule both end as the same [`Effect::Resolve`], carried by the same
/// id, run by the same driver — which is what makes a headless run answer the
/// agent exactly as the terminal would have. The decision returned is the one
/// the agent heard (see [`Prompt::answer`]).
pub fn decide(policy: &Policy, prompt: &Prompt) -> (Decision, Effect) {
    let (decision, outcome) = prompt.answer(policy.decide(prompt));
    (decision, answer_with(prompt, outcome))
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
        effects.push(answer_with(prompt, prompt.unanswered()));
    }
    for prompt in std::mem::take(&mut state.queued) {
        effects.push(answer_with(&prompt, prompt.unanswered()));
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
    match resolve(&state.commands, &state.prompt_commands, &line) {
        Resolution::Owned {
            action: "commands", ..
        } => {
            effects.push(Effect::Notice(Notice::Commands));
        }
        Resolution::Owned {
            action: "route_set",
            ..
        } => {
            effects.push(Effect::ListRoutes);
            return effects;
        }
        Resolution::Owned {
            action: "route_reset",
            ..
        } => {
            effects.push(Effect::ResetRoute);
            return effects;
        }
        // `REDUCER_OWNED` names the ids matched above, and a guard pins the
        // two together. An id arriving here is a command list the app built
        // wrongly, and is said as such rather than silently sent as a prompt.
        Resolution::Owned { action, .. } => {
            effects.push(Effect::Notice(Notice::Say(format!(
                "`{action}` is marked reducer-owned but has no reducer arm"
            ))));
        }
        Resolution::Action { action, args } => {
            effects.push(Effect::Action { action, args });
            return effects;
        }
        Resolution::Unavailable(reason) => {
            effects.push(Effect::Notice(Notice::Say(reason.to_string())));
        }
        // An expansion is a prompt: the turn is what runs, and the journal
        // records the text actually sent rather than what was typed.
        Resolution::Expand(prompt) | Resolution::Prompt(prompt) => {
            state.prompts = state.prompts.saturating_add(1);
            state.phase = Phase::Turn;
            effects.push(Effect::Prompt {
                line: prompt,
                nth: state.prompts,
            });
        }
    }
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
    // Ctrl-L asks for the screen back, not for a decision. It is the one
    // control chord that is not a decline, because it does not mean anything
    // about the question.
    if crate::editor::is_redraw(event) {
        state.phase = Phase::Answering(prompt);
        return vec![Effect::Redraw];
    }
    // Every other control chord is never a choice: Ctrl-C must answer the
    // question the only way an interrupt can be read — no — rather than
    // selecting whatever `c` happens to be numbered.
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
        answer_with(&prompt, outcome),
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
    // Ctrl-L asks for the screen back, and the picker stays up: it says
    // nothing about which route to take.
    if crate::editor::is_redraw(event) {
        state.phase = Phase::Routing(picker);
        return vec![Effect::Redraw];
    }
    // Every other control chord is never a choice — Ctrl-C closes the picker
    // instead of selecting whatever `c` happens to be.
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Esc {
        return vec![
            Effect::Modal(None),
            Effect::Notice(Notice::Say(ROUTE_UNCHANGED.to_string())),
            Effect::Paint(Trigger::Key),
        ];
    }
    // A digit selects; anything else printable narrows the list. Model ids do
    // contain digits, so filtering by one is lost — but `glm-4.7` is still
    // reachable by typing `glm` and then picking the number beside it, and the
    // alternative is a keystroke whose meaning depends on how many routes the
    // daemon happened to return.
    let mut picker = picker;
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if let Some(route) = picker.choose(c) {
                // A route to *attempt*. Only what the daemon confirms is in
                // force.
                return vec![Effect::Modal(None), Effect::SetRoute(route)];
            }
            // A number nothing is drawn beside chooses nothing, rather than
            // wrapping around to whatever happens to be at that index.
            state.phase = Phase::Routing(picker);
            Vec::new()
        }
        KeyCode::Char(c) => {
            picker.filter(c);
            let row = picker.render();
            state.phase = Phase::Routing(picker);
            vec![Effect::Modal(Some(row)), Effect::Paint(Trigger::Key)]
        }
        KeyCode::Backspace => {
            picker.unfilter();
            let row = picker.render();
            state.phase = Phase::Routing(picker);
            vec![Effect::Modal(Some(row)), Effect::Paint(Trigger::Key)]
        }
        _ => {
            state.phase = Phase::Routing(picker);
            Vec::new()
        }
    }
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
                answer_with(&prompt, prompt.unanswered()),
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
    // The gate is asked again here so there is no way to draw a picker without
    // answering it.
    let Some(picker) = Picker::open(
        state.available("route_set"),
        &listed.available,
        listed.current.as_deref(),
    ) else {
        return vec![
            Effect::Notice(Notice::Say(NO_ROUTES.to_string())),
            Effect::Paint(Trigger::Key),
        ];
    };
    let row = picker.render();
    state.phase = Phase::Routing(picker);
    vec![Effect::Modal(Some(row)), Effect::Paint(Trigger::Key)]
}

/// `_bitrouter/route/set` or `/reset` came back. What it *confirmed* is what is
/// reported, never what was asked for: `set` can legitimately refuse.
fn routed(installed: Result<Option<String>, String>) -> Vec<Effect> {
    match installed {
        Ok(Some(in_force)) => vec![
            // The footer names the route for the rest of the session, not just
            // for this frame.
            Effect::RouteInForce(Some(in_force.clone())),
            Effect::Notice(Notice::Say(format!("route: {in_force}"))),
            Effect::Paint(Trigger::Key),
        ],
        // The lease is gone, so the footer must stop naming a route: what is in
        // force is whatever the daemon decides per turn.
        Ok(None) => vec![
            Effect::RouteInForce(None),
            Effect::Notice(Notice::Say(ROUTE_RESET.to_string())),
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

    /// The command list the app hands a session, as these tests need it: the
    /// three BitRouter commands, with the two route verbs gated the way a
    /// controller with — or without — route control would gate them.
    fn commands_for(routable: bool) -> Vec<Command> {
        let unavailable = (!routable).then_some("this session cannot be rerouted");
        vec![
            Command {
                name: "commands",
                action: "commands",
                summary: "list the commands this session offers",
                unavailable: None,
            },
            Command {
                name: "route",
                action: "route_set",
                summary: "choose the route for the rest of the session",
                unavailable,
            },
            Command {
                name: "route reset",
                action: "route_reset",
                summary: "drop the route lease",
                unavailable,
            },
        ]
    }

    /// A question offering allow / always / reject, in that order.
    fn question(id: &str) -> Prompt {
        Prompt::new(
            id,
            Some("Write src/main.rs".to_string()),
            "t1",
            Some(agent_client_protocol_schema::v1::ToolKind::Edit),
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
                Effect::ResetRoute => "reset-route",
                Effect::Action { .. } => "action",
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
        let mut state = State::new(commands_for(true));
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
    /// The `Ctrl-L` row is the one cell that changed rather than being
    /// preserved. Both modals used to read *any* control chord as a cancel, so
    /// asking for a redraw answered the agent's question with "no". Nobody
    /// decided that; it fell out of one `if` in each of two functions, and it
    /// was invisible until the table put the rows next to each other.
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
            // Ctrl-L: redraw, in every phase, and the modal stays up. It is
            // the one control chord the two modals do not read as a cancel,
            // because it says nothing about the question they are asking.
            ("idle", ctrl('l'), &["echo", "redraw"], "idle"),
            ("turn", ctrl('l'), &["redraw"], "turn"),
            ("answering", ctrl('l'), &["redraw"], "answering"),
            ("routing", ctrl('l'), &["redraw"], "routing"),
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
            // Any other printable key: types it / ignored / nothing /
            // narrows the route list.
            (
                "idle",
                press(KeyCode::Char('x')),
                &["echo", "paint"],
                "idle",
            ),
            ("turn", press(KeyCode::Char('x')), &[], "turn"),
            ("answering", press(KeyCode::Char('x')), &[], "answering"),
            (
                "routing",
                press(KeyCode::Char('x')),
                &["modal", "paint"],
                "routing",
            ),
            // Backspace: the picker is the only phase that reads it as its
            // own, widening the filter again rather than editing a line that
            // is not on screen.
            (
                "idle",
                press(KeyCode::Backspace),
                &["echo", "paint"],
                "idle",
            ),
            ("turn", press(KeyCode::Backspace), &[], "turn"),
            ("answering", press(KeyCode::Backspace), &[], "answering"),
            (
                "routing",
                press(KeyCode::Backspace),
                &["modal", "paint"],
                "routing",
            ),
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
        for event in [ctrl('c'), press(KeyCode::Esc), ctrl('d')] {
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
                Action::Routed(Ok(Some("@balanced".to_string()))),
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
        let mut state = State::new(commands_for(true));
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

    /// Precedence, stated once and asserted here: the longest BitRouter name
    /// wins, an alias reaches its target, an unmet requirement answers with its
    /// reason, and anything else is the agent's business untouched.
    #[test]
    fn the_resolver_reads_the_longest_name_first_then_aliases_then_the_agent() {
        let offered = commands_for(true);
        assert_eq!(
            resolve(&offered, &[], "/route reset"),
            Resolution::Owned {
                action: "route_reset",
                args: Vec::new()
            },
            "`/route reset` is one two-word name, not `/route` with an argument"
        );
        assert_eq!(
            resolve(&offered, &[], "/route"),
            Resolution::Owned {
                action: "route_set",
                args: Vec::new()
            },
            "the one-word name is still reachable once the two-word one misses"
        );
        assert_eq!(
            resolve(&offered, &[], "/help"),
            Resolution::Owned {
                action: "commands",
                args: Vec::new()
            },
            "an alias reaches its target"
        );
        assert_eq!(
            resolve(&offered, &[], "/plan ship it"),
            Resolution::Prompt("/plan ship it".to_string()),
            "a command we do not offer is the agent's, and is passed through whole"
        );
        assert_eq!(
            resolve(&offered, &[], "hello"),
            Resolution::Prompt("hello".to_string()),
            "a bare line is a prompt"
        );
        // Arguments are split, never handed over as one string.
        assert_eq!(
            resolve(&offered, &[], "/route  us-east   fast "),
            Resolution::Owned {
                action: "route_set",
                args: vec!["us-east".to_string(), "fast".to_string()]
            }
        );
        // A listed-but-unrunnable command answers with its reason.
        match resolve(&commands_for(false), &[], "/route") {
            Resolution::Unavailable(reason) => {
                assert!(reason.contains("cannot be rerouted"), "got `{reason}`")
            }
            other => panic!("expected the reason, got {other:?}"),
        }
    }

    /// A config command expands and is sent as a turn; a BitRouter name still
    /// wins, even against a config command that claims it.
    #[test]
    fn a_prompt_command_expands_and_never_outranks_a_bitrouter_name() {
        let offered = commands_for(true);
        let configured = vec![
            PromptCommand {
                name: "review".to_string(),
                description: "review a diff".to_string(),
                template: "Review this: $ARGUMENTS".to_string(),
            },
            // A config that names a BitRouter command is refused at load, so
            // this can only arise from a bug; the resolver must still not
            // prefer it.
            PromptCommand {
                name: "route".to_string(),
                description: "should never win".to_string(),
                template: "nope".to_string(),
            },
        ];
        assert_eq!(
            resolve(&offered, &configured, "/review the diff"),
            Resolution::Expand("Review this: the diff".to_string())
        );
        // No arguments substitutes the empty string rather than leaving the
        // placeholder visible in what is sent.
        assert_eq!(
            resolve(&offered, &configured, "/review"),
            Resolution::Expand("Review this: ".to_string())
        );
        assert_eq!(
            resolve(&offered, &configured, "/route"),
            Resolution::Owned {
                action: "route_set",
                args: Vec::new()
            },
            "local wins: the closed table is consulted before the open one"
        );
        assert_eq!(
            resolve(&offered, &configured, "/unknown thing"),
            Resolution::Prompt("/unknown thing".to_string()),
            "neither registry claims it, so it is the agent's"
        );
    }

    /// `/route reset` drops the lease, and what comes back clears the footer
    /// rather than naming a route nothing is holding.
    #[test]
    fn resetting_the_route_clears_what_the_footer_names() {
        let mut state = State::new(commands_for(true));
        for c in "/route reset".chars() {
            let _ = step(&mut state, Action::Key(press(KeyCode::Char(c))));
        }
        let effects = step(&mut state, Action::Key(press(KeyCode::Enter)));
        assert_eq!(
            effects_of(&effects),
            ["echo", "clear-notice", "reset-route"]
        );

        let effects = step(&mut state, Action::Routed(Ok(None)));
        assert_eq!(effects_of(&effects), ["route-in-force", "notice", "paint"]);
        assert!(
            matches!(effects.first(), Some(Effect::RouteInForce(None))),
            "a dropped lease must stop the footer naming a route"
        );
    }

    /// `/route` is gated on the capability the controller advertised, and the
    /// answer when it is absent is a sentence rather than a dead picker.
    #[test]
    fn route_needs_the_capability_the_controller_advertised() {
        for (routable, expected) in [(true, "list-routes"), (false, "notice")] {
            let mut state = State::new(commands_for(routable));
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
        let mut state = State::new(commands_for(true));
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
        let effects = step(
            &mut state,
            Action::Routed(Ok(Some("@balanced".to_string()))),
        );
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
            let mut state = State::new(commands_for(routable));
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
        let mut state = State::new(commands_for(true));
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

    /// A headless policy answers with the agent's own option, as a `Resolve`
    /// for the question's id — the effect a keystroke would have produced.
    #[test]
    fn a_headless_policy_answers_with_the_agents_own_option() {
        use crate::permission::Mode;

        let prompt = question("r1");
        let approve = Policy {
            mode: Mode::ApproveAll,
            ..Policy::default()
        };
        let (decision, effect) = decide(&approve, &prompt);
        assert_eq!(decision, Decision::Approve);
        assert_eq!(answered_with(&[effect], "r1").as_deref(), Some("allow"));

        let (decision, effect) = decide(&Policy::default(), &prompt);
        assert_eq!(decision, Decision::Deny);
        assert_eq!(answered_with(&[effect], "r1").as_deref(), Some("no"));
    }

    /// A policy that cannot approve — the agent offered no allow option —
    /// selects the reject option and says so, so an exit status built on the
    /// decision counts what the agent heard.
    #[test]
    fn a_policy_that_cannot_approve_reports_deny() {
        use crate::permission::Mode;

        let approve = Policy {
            mode: Mode::ApproveAll,
            ..Policy::default()
        };
        let reject_only = Prompt::new(
            "r1",
            Some("Write src/main.rs".to_string()),
            "t1",
            None,
            vec![option("no", PermissionOptionKind::RejectOnce)],
        );
        let (decision, effect) = decide(&approve, &reject_only);
        assert_eq!(decision, Decision::Deny);
        assert_eq!(answered_with(&[effect], "r1").as_deref(), Some("no"));

        let nothing = Prompt::new("r2", None, "t2", None, Vec::new());
        let (decision, effect) = decide(&approve, &nothing);
        assert_eq!(decision, Decision::Deny);
        assert!(matches!(
            effect,
            Effect::Resolve {
                outcome: RequestPermissionOutcome::Cancelled,
                ..
            }
        ));
    }
}
