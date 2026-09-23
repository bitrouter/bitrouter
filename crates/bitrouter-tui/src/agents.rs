//! Reducer and renderers for supervised background-agent presentation.
//!
//! The application maps supervisor snapshots into the view data in this
//! module and executes the typed effects returned by [`AgentDeckState`]. This
//! crate never discovers processes, reads a run ledger, or decides whether a
//! mutation is authorized. In particular, a local lease shown here is only a
//! fact supplied by the supervisor; every mutating effect still carries its
//! generation and a stable action request id for server-side validation.

use std::collections::HashMap;
use std::io::{self, IsTerminal};

use crossterm::cursor::Hide;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::editor::{Edit, Editor};
use crate::wrap::wrap;

/// How the agent deck is hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDeckMode {
    /// A bounded dock below one foreground Code conversation.
    Inline,
    /// The explicitly launched, alternate-screen `bro agents` manager.
    Standalone,
}

/// Supervisor-owned process lifetime projected for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProcessState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    Interrupted,
}

/// Supervisor-owned turn lifetime projected for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTurnState {
    Idle,
    Submitting,
    Working,
    Cancelling,
}

/// Whether a settled result still needs review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentReviewState {
    Unread,
    Reviewed,
}

/// One exact controller-offered permission option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPermissionOption {
    pub id: String,
    pub label: String,
}

/// Exact pending permission data safe to project into a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPermissionView {
    pub permission_id: String,
    pub title: String,
    /// Sanitized retained command/diff/location/policy context. Non-empty
    /// detail is reviewed in the scrollable inspector, never clipped inline.
    pub detail: String,
    pub options: Vec<AgentPermissionOption>,
    /// True when the complete context must be reviewed in the inspector.
    pub requires_inspector: bool,
}

/// Orthogonal attention state. Payloads are supplied, not inferred, by the
/// application mapping from supervisor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAttention {
    None,
    Question {
        question_id: String,
        title: String,
        detail: String,
    },
    Permission(AgentPermissionView),
    Result {
        summary: String,
    },
    Error {
        message: String,
    },
}

/// Current single-writer lease projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentLeaseView {
    pub generation: u64,
    pub owner: Option<String>,
    /// Set by the application after comparing the authenticated client id.
    pub owned_by_client: bool,
}

/// One supervised run, already reduced to presentation facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunView {
    pub run_id: String,
    pub native_session_id: Option<String>,
    pub label: String,
    pub agent: String,
    pub directory: String,
    pub parent_run_id: Option<String>,
    pub process: AgentProcessState,
    pub turn: AgentTurnState,
    pub attention: AgentAttention,
    pub review: AgentReviewState,
    pub activity: String,
    pub confirmed_route: Option<String>,
    pub attributed_cost: Option<String>,
    pub failure: Option<String>,
    pub lease: AgentLeaseView,
    pub pinned: bool,
    /// Coarse event-derived age supplied by the application. It is shown only
    /// while expanded and never drives a repaint timer in this reducer.
    pub age_label: Option<String>,
    pub last_seq: u64,
}

impl AgentRunView {
    /// Construct a running, idle row with honest unknown optional facts.
    pub fn new(
        run_id: impl Into<String>,
        label: impl Into<String>,
        agent: impl Into<String>,
        directory: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            native_session_id: None,
            label: label.into(),
            agent: agent.into(),
            directory: directory.into(),
            parent_run_id: None,
            process: AgentProcessState::Running,
            turn: AgentTurnState::Idle,
            attention: AgentAttention::None,
            review: AgentReviewState::Reviewed,
            activity: "Idle".to_string(),
            confirmed_route: None,
            attributed_cost: None,
            failure: None,
            lease: AgentLeaseView::default(),
            pinned: false,
            age_label: None,
            last_seq: 0,
        }
    }
}

/// Application-supplied defaults for the bounded new-run editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentRunTarget {
    pub agent: String,
    pub directory: String,
    pub route: Option<String>,
    pub conflict: Option<String>,
}

/// One canonical directory choice and any supervisor/app preflight conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentDirectoryChoice {
    pub directory: String,
    pub conflict: Option<String>,
}

/// Application-supplied safe choices for the bounded new-run target editor.
/// The reducer never reads configuration or the filesystem to create them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewAgentRunChoices {
    pub agents: Vec<String>,
    pub directories: Vec<NewAgentDirectoryChoice>,
    /// `None` is the application-confirmed default route.
    pub routes: Vec<Option<String>>,
}

/// Atomic list projection from the supervisor client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDeckSnapshot {
    pub sequence: u64,
    pub runs: Vec<AgentRunView>,
    pub new_run_target: Option<NewAgentRunTarget>,
}

/// Retained inspector event kind. The text remains exact application-supplied
/// content; this renderer never asks another model to summarize it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHistoryKind {
    User,
    Assistant,
    Thought,
    Tool,
    Status,
    Permission,
    Result,
    Error,
}

/// One monotonically sequenced retained display event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHistoryEvent {
    pub seq: u64,
    pub kind: AgentHistoryKind,
    pub text: String,
}

/// Atomic attach payload mapped from `RunAttachment`/`ReplayBatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHistorySnapshot {
    pub run: AgentRunView,
    pub first_retained_seq: u64,
    pub snapshot_seq: u64,
    pub history_complete: bool,
    pub events: Vec<AgentHistoryEvent>,
}

/// Why a transient lease is being acquired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLeaseIntent {
    Reply,
    Permission,
    Cancel,
    MarkReviewed,
    Stop,
    Attach,
}

/// Code launcher actions for the currently selected supervised run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDeckCommand {
    Open,
    Reply,
    New,
    Attach,
    Peek,
    ReviewPermission,
    Takeover,
    Cancel,
    Stop,
    MarkReviewed,
    Detach,
    Filter,
    ToggleStopped,
    Search,
    Copy,
    Export,
}

/// Typed work for the application/supervisor client. No variant performs work
/// inside this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEffect {
    Refresh,
    AcquireLease {
        run_id: String,
        intent: AgentLeaseIntent,
        action_request_id: String,
    },
    ReleaseLease {
        run_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    Reply {
        run_id: String,
        prompt: String,
        lease_generation: u64,
        action_request_id: String,
    },
    RespondPermission {
        run_id: String,
        permission_id: String,
        option_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    NewRun {
        target: NewAgentRunTarget,
        prompt: String,
        action_request_id: String,
    },
    CancelTurn {
        run_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    MarkReviewed {
        run_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    /// Deliberately replace another client's lease and enter the inspector.
    /// The supervisor owns generation advancement and audit recording.
    Takeover {
        run_id: String,
        action_request_id: String,
    },
    Stop {
        run_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    Attach {
        run_id: String,
        action_request_id: String,
    },
    Detach {
        run_id: String,
        lease_generation: u64,
        action_request_id: String,
    },
    Resync {
        run_id: String,
        expected_seq: u64,
        received_seq: u64,
    },
    Copy {
        text: String,
    },
    Export {
        run_id: String,
        content: String,
    },
    ExitStandalone,
}

impl AgentEffect {
    /// Whether a newly pending foreground permission must prevent this effect
    /// from starting. Read-only refresh/detail work and lease release remain
    /// available so the UI can recover safely.
    pub fn blocked_by_foreground_permission(&self) -> bool {
        matches!(
            self,
            Self::AcquireLease { .. }
                | Self::Reply { .. }
                | Self::RespondPermission { .. }
                | Self::NewRun { .. }
                | Self::CancelTurn { .. }
                | Self::MarkReviewed { .. }
                | Self::Takeover { .. }
                | Self::Stop { .. }
                | Self::Attach { .. }
        )
    }

    pub fn action_request_id(&self) -> Option<&str> {
        match self {
            Self::AcquireLease {
                action_request_id, ..
            }
            | Self::ReleaseLease {
                action_request_id, ..
            }
            | Self::Reply {
                action_request_id, ..
            }
            | Self::RespondPermission {
                action_request_id, ..
            }
            | Self::NewRun {
                action_request_id, ..
            }
            | Self::CancelTurn {
                action_request_id, ..
            }
            | Self::MarkReviewed {
                action_request_id, ..
            }
            | Self::Takeover {
                action_request_id, ..
            }
            | Self::Stop {
                action_request_id, ..
            }
            | Self::Attach {
                action_request_id, ..
            }
            | Self::Detach {
                action_request_id, ..
            } => Some(action_request_id),
            Self::Refresh
            | Self::Resync { .. }
            | Self::Copy { .. }
            | Self::Export { .. }
            | Self::ExitStandalone => None,
        }
    }
}

/// Application acknowledgements and terminal input accepted by the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Event(Event),
    #[cfg(test)]
    TestLegacyKey(KeyCode),
    LeaseAcquired {
        run_id: String,
        intent: AgentLeaseIntent,
        generation: u64,
        action_request_id: String,
    },
    MutationAccepted {
        action_request_id: String,
    },
    EffectFailed {
        action_request_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AgentGroup {
    NeedsInput,
    Ready,
    Working,
    Idle,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentSurface {
    Collapsed,
    List,
    Peek,
    Reply {
        run_id: String,
    },
    NewRun,
    Permission {
        run_id: String,
        permission_id: String,
        lease_generation: u64,
        selected: Option<usize>,
    },
    Confirm {
        run_id: String,
        intent: AgentLeaseIntent,
    },
    TakeoverConfirm {
        run_id: String,
        selected: bool,
    },
    ExitConfirm {
        selected: Option<usize>,
    },
    Inspector(Box<AgentInspector>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewRunFocus {
    Agent,
    Directory,
    Route,
    Prompt,
}

impl NewRunFocus {
    fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::Agent, false) | (Self::Route, true) => Self::Directory,
            (Self::Directory, false) | (Self::Prompt, true) => Self::Route,
            (Self::Route, false) | (Self::Agent, true) => Self::Prompt,
            (Self::Prompt, false) | (Self::Directory, true) => Self::Agent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentInspector {
    run: AgentRunView,
    first_retained_seq: u64,
    snapshot_seq: u64,
    history_complete: bool,
    events: Vec<AgentHistoryEvent>,
    scroll: usize,
    search: String,
    searching: bool,
    permission_focused: bool,
    permission_selected: Option<usize>,
    permission_scroll: usize,
}

impl AgentInspector {
    fn content(&self) -> String {
        visible_inspector_content(&self.run, &self.events, self.history_complete)
    }

    fn last_content_line(&self) -> usize {
        self.content().lines().count().saturating_sub(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisibleHistoryBlock {
    kind: AgentHistoryKind,
    first_seq: u64,
    last_seq: u64,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAction {
    id: String,
    run_id: Option<String>,
    intent: Option<AgentLeaseIntent>,
    stage: PendingStage,
    abandoned: bool,
    clears_reply: bool,
    clears_new_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingStage {
    LeaseAcquisition,
    Mutation,
}

#[derive(Debug, Default)]
struct NewRunTargetEditors {
    agent: Editor,
    directory: Editor,
    route: Editor,
}

/// Pure client state for the collapsed strip, expanded command center, and
/// retained-history inspector.
#[derive(Debug)]
pub struct AgentDeckState {
    mode: AgentDeckMode,
    client_id: String,
    snapshot: AgentDeckSnapshot,
    foreground_run_id: Option<String>,
    selected_run_id: Option<String>,
    list_scroll: usize,
    filter: String,
    filtering: bool,
    show_stopped: bool,
    surface: AgentSurface,
    reply_drafts: HashMap<String, Editor>,
    new_run_draft: Editor,
    new_run_choices: NewAgentRunChoices,
    new_run_target: Option<NewAgentRunTarget>,
    new_run_target_editors: NewRunTargetEditors,
    new_run_focus: NewRunFocus,
    pending: Option<PendingAction>,
    inspector_run_id: Option<String>,
    next_action: u64,
    notice: Option<String>,
}

impl AgentDeckState {
    /// Invoke a named Code action through the same guarded paths as the deck.
    pub fn command(
        &mut self,
        command: AgentDeckCommand,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        match command {
            AgentDeckCommand::Open => {
                if self.is_collapsed() {
                    self.toggle();
                }
                Vec::new()
            }
            AgentDeckCommand::Reply => self.begin_reply(foreground_permission_pending),
            AgentDeckCommand::New => {
                if self.mutations_blocked(foreground_permission_pending) {
                    return Vec::new();
                }
                let target = self
                    .default_new_run_target()
                    .or_else(|| self.snapshot.new_run_target.clone())
                    .unwrap_or_else(|| NewAgentRunTarget {
                        agent: String::new(),
                        directory: String::new(),
                        route: None,
                        conflict: None,
                    });
                self.set_new_run_target(target);
                self.new_run_focus = NewRunFocus::Prompt;
                self.surface = AgentSurface::NewRun;
                Vec::new()
            }
            AgentDeckCommand::Attach => self.begin_attach(foreground_permission_pending),
            AgentDeckCommand::Peek => {
                if self.selected_run().is_some() {
                    self.surface = AgentSurface::Peek;
                }
                Vec::new()
            }
            AgentDeckCommand::ReviewPermission => {
                if let AgentSurface::Inspector(inspector) = &mut self.surface {
                    if foreground_permission_pending {
                        self.notice = Some("Resolve the foreground permission first".to_string());
                    } else if matches!(&inspector.run.attention, AgentAttention::Permission(_)) {
                        inspector.permission_focused = true;
                        inspector.permission_selected = None;
                        inspector.permission_scroll = 0;
                    }
                    Vec::new()
                } else {
                    self.begin_permission(foreground_permission_pending)
                }
            }
            AgentDeckCommand::Takeover => {
                self.begin_takeover(foreground_permission_pending);
                Vec::new()
            }
            AgentDeckCommand::Cancel => {
                self.begin_confirmation(AgentLeaseIntent::Cancel, foreground_permission_pending)
            }
            AgentDeckCommand::Stop => {
                self.begin_confirmation(AgentLeaseIntent::Stop, foreground_permission_pending)
            }
            AgentDeckCommand::MarkReviewed => self.begin_confirmation(
                AgentLeaseIntent::MarkReviewed,
                foreground_permission_pending,
            ),
            AgentDeckCommand::Detach => self.f5(),
            AgentDeckCommand::Filter => {
                self.filtering = true;
                Vec::new()
            }
            AgentDeckCommand::ToggleStopped => {
                self.show_stopped = !self.show_stopped;
                self.ensure_selection();
                Vec::new()
            }
            AgentDeckCommand::Search => {
                if let AgentSurface::Inspector(inspector) = &mut self.surface {
                    inspector.searching = true;
                }
                Vec::new()
            }
            AgentDeckCommand::Copy => match &self.surface {
                AgentSurface::Inspector(inspector) => vec![AgentEffect::Copy {
                    text: inspector.content(),
                }],
                _ => Vec::new(),
            },
            AgentDeckCommand::Export => match &self.surface {
                AgentSurface::Inspector(inspector) => vec![AgentEffect::Export {
                    run_id: inspector.run.run_id.clone(),
                    content: inspector.content(),
                }],
                _ => Vec::new(),
            },
        }
    }

    /// Explain why a launcher row cannot currently act on the selected run.
    pub fn command_unavailable(
        &self,
        command: AgentDeckCommand,
        foreground_permission_pending: bool,
    ) -> Option<&'static str> {
        if matches!(command, AgentDeckCommand::Open) {
            return None;
        }
        if matches!(command, AgentDeckCommand::Detach) {
            return (!self.is_inspector()).then_some("No attached run to detach");
        }
        if matches!(
            command,
            AgentDeckCommand::Filter | AgentDeckCommand::ToggleStopped
        ) {
            return (!matches!(self.surface, AgentSurface::List))
                .then_some("Open the background run list first");
        }
        if matches!(
            command,
            AgentDeckCommand::Search | AgentDeckCommand::Copy | AgentDeckCommand::Export
        ) {
            return (!self.is_inspector()).then_some("Attach a run inspector first");
        }
        if foreground_permission_pending
            && matches!(
                command,
                AgentDeckCommand::Reply
                    | AgentDeckCommand::New
                    | AgentDeckCommand::Attach
                    | AgentDeckCommand::Takeover
                    | AgentDeckCommand::Cancel
                    | AgentDeckCommand::Stop
                    | AgentDeckCommand::MarkReviewed
                    | AgentDeckCommand::ReviewPermission
            )
        {
            return Some("Resolve the foreground permission first");
        }
        if matches!(command, AgentDeckCommand::New) {
            return None;
        }
        let Some(run) = self.selected_run() else {
            return Some("Select a background run first");
        };
        if matches!(command, AgentDeckCommand::ReviewPermission)
            && !matches!(run.attention, AgentAttention::Permission(_))
        {
            return Some("Selected run has no permission to review");
        }
        None
    }

    pub fn new(mode: AgentDeckMode, client_id: impl Into<String>) -> Self {
        let surface = match mode {
            AgentDeckMode::Inline => AgentSurface::Collapsed,
            AgentDeckMode::Standalone => AgentSurface::List,
        };
        Self {
            mode,
            client_id: client_id.into(),
            snapshot: AgentDeckSnapshot::default(),
            foreground_run_id: None,
            selected_run_id: None,
            list_scroll: 0,
            filter: String::new(),
            filtering: false,
            show_stopped: false,
            surface,
            reply_drafts: HashMap::new(),
            new_run_draft: Editor::default(),
            new_run_choices: NewAgentRunChoices::default(),
            new_run_target: None,
            new_run_target_editors: NewRunTargetEditors::default(),
            new_run_focus: NewRunFocus::Prompt,
            pending: None,
            inspector_run_id: None,
            next_action: 0,
            notice: None,
        }
    }

    pub fn snapshot(&self) -> &AgentDeckSnapshot {
        &self.snapshot
    }

    /// Bind locally generated action ids and lease ownership to the
    /// authenticated supervisor client. Call this before the first snapshot or
    /// effect. Changing it later invalidates pending local confirmation state.
    pub fn set_client_id(&mut self, client_id: impl Into<String>) {
        let client_id = client_id.into();
        if self.client_id == client_id {
            return;
        }
        self.client_id = client_id;
        self.pending = None;
        self.inspector_run_id = None;
        if matches!(
            self.surface,
            AgentSurface::Permission { .. } | AgentSurface::Inspector(_)
        ) {
            self.surface = AgentSurface::List;
        }
        self.notice = Some("Agent client identity changed · refresh required".to_string());
    }

    pub fn selected_run_id(&self) -> Option<&str> {
        self.selected_run_id.as_deref()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_expanded(&self) -> bool {
        !matches!(
            self.surface,
            AgentSurface::Collapsed | AgentSurface::Inspector(_)
        )
    }

    pub fn is_inspector(&self) -> bool {
        matches!(self.surface, AgentSurface::Inspector(_))
    }

    /// Whether Code may consume a leading slash for its command launcher.
    /// Text fields keep literal slashes away from their first byte unchanged.
    pub fn slash_opens_commands(&self) -> bool {
        match &self.surface {
            AgentSurface::Reply { run_id } => self
                .reply_drafts
                .get(run_id)
                .is_none_or(|draft| draft.cursor_byte() == 0),
            AgentSurface::NewRun => match self.new_run_focus {
                NewRunFocus::Agent => self.new_run_target_editors.agent.cursor_byte() == 0,
                NewRunFocus::Directory => self.new_run_target_editors.directory.cursor_byte() == 0,
                NewRunFocus::Route => self.new_run_target_editors.route.cursor_byte() == 0,
                NewRunFocus::Prompt => self.new_run_draft.cursor_byte() == 0,
            },
            AgentSurface::List => !self.filtering,
            AgentSurface::Inspector(inspector) => !inspector.searching,
            AgentSurface::Peek
            | AgentSurface::Permission { .. }
            | AgentSurface::Confirm { .. }
            | AgentSurface::TakeoverConfirm { .. }
            | AgentSurface::ExitConfirm { .. } => true,
            AgentSurface::Collapsed => false,
        }
    }

    /// Whether the current inline deck focus is an editable text field.
    pub fn slash_targets_text(&self) -> bool {
        matches!(
            self.surface,
            AgentSurface::Reply { .. } | AgentSurface::NewRun
        )
    }

    /// Insert the escape form `//` as one literal slash in the focused field.
    pub fn insert_literal_slash(&mut self) {
        match &self.surface {
            AgentSurface::Reply { run_id } => {
                self.reply_drafts
                    .entry(run_id.clone())
                    .or_default()
                    .paste("/");
            }
            AgentSurface::NewRun => {
                if self.new_run_focus == NewRunFocus::Prompt {
                    self.new_run_draft.paste("/");
                } else {
                    self.paste_new_run_target("/");
                }
            }
            _ => {}
        }
    }

    pub fn is_collapsed(&self) -> bool {
        matches!(self.surface, AgentSurface::Collapsed)
    }

    pub fn has_background_runs(&self) -> bool {
        self.snapshot.runs.iter().any(|run| {
            self.foreground_run_id.as_deref() != Some(run.run_id.as_str())
                && group(run) != AgentGroup::Stopped
        })
    }

    /// Whether leaving this client would discard an unsent target-bound draft.
    pub fn has_unsent_drafts(&self) -> bool {
        !self.new_run_draft.is_empty()
            || self.reply_drafts.values().any(|editor| !editor.is_empty())
    }

    /// Name the currently inline supervisor run so it never appears in the
    /// background strip, groups, or counts even when list snapshots include it.
    pub fn set_foreground_run_id(&mut self, run_id: Option<String>) {
        self.foreground_run_id = run_id;
        self.ensure_selection();
    }

    /// Supply user-selectable target facts without giving the TUI config or
    /// filesystem ownership.
    pub fn set_new_run_choices(&mut self, choices: NewAgentRunChoices) {
        self.new_run_choices = choices;
        if self.new_run_target.is_none() {
            self.new_run_target = self.default_new_run_target();
        }
    }

    /// Replace the list projection while retaining selection, filter, and
    /// target-bound drafts. The return value says whether the *collapsed strip*
    /// changed, so a driver can avoid a terminal write for token/tool deltas
    /// that do not change ambient awareness.
    pub fn replace_snapshot(&mut self, snapshot: AgentDeckSnapshot) -> bool {
        let old_summary = self.collapsed_summary();
        let permission_identity = self.focused_permission_identity();
        self.snapshot = snapshot;
        if let AgentSurface::Inspector(inspector) = &mut self.surface
            && let Some(run) = self
                .snapshot
                .runs
                .iter()
                .find(|run| run.run_id == inspector.run.run_id)
        {
            inspector.run = run.clone();
            if !matches!(inspector.run.attention, AgentAttention::Permission(_)) {
                inspector.permission_focused = false;
                inspector.permission_selected = None;
                inspector.permission_scroll = 0;
            }
        }
        self.ensure_selection();
        if permission_identity != self.focused_permission_identity()
            && matches!(self.surface, AgentSurface::Permission { .. })
        {
            self.surface = AgentSurface::Peek;
        }
        let valid_ids = self
            .snapshot
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>();
        self.reply_drafts
            .retain(|run_id, _| valid_ids.iter().any(|valid| *valid == run_id));
        old_summary != self.collapsed_summary()
    }

    /// Replace inspector history atomically. Replayed events at or below
    /// `snapshot_seq` are accepted exactly once after sorting and de-duplication.
    pub fn replace_history(&mut self, mut snapshot: AgentHistorySnapshot) -> bool {
        if self.inspector_run_id.as_deref() != Some(snapshot.run.run_id.as_str()) {
            return false;
        }
        snapshot.events.retain(|event| {
            event.seq >= snapshot.first_retained_seq && event.seq <= snapshot.snapshot_seq
        });
        snapshot.events.sort_by_key(|event| event.seq);
        snapshot.events.dedup_by_key(|event| event.seq);
        self.selected_run_id = Some(snapshot.run.run_id.clone());
        let inspector = AgentInspector {
            run: snapshot.run,
            first_retained_seq: snapshot.first_retained_seq,
            snapshot_seq: snapshot.snapshot_seq,
            history_complete: snapshot.history_complete,
            events: snapshot.events,
            scroll: 0,
            search: String::new(),
            searching: false,
            permission_focused: false,
            permission_selected: None,
            permission_scroll: 0,
        };
        let scroll = inspector.last_content_line();
        self.surface = AgentSurface::Inspector(Box::new(AgentInspector {
            scroll,
            ..inspector
        }));
        self.notice = None;
        true
    }

    /// Authorize the next atomic replay for an app-requested initial attach.
    /// Interactive attach/takeover paths set this internally. A driver that
    /// opens directly by run id must call this before delivering history.
    pub fn expect_inspector_history(&mut self, run_id: impl Into<String>) {
        self.inspector_run_id = Some(run_id.into());
    }

    /// Decline an attach result that became unsafe before delivery, for
    /// example because a foreground permission arrived while the RPC was in
    /// flight. The application routes the returned fenced detach normally.
    pub fn decline_inspector_history(
        &mut self,
        snapshot: &AgentHistorySnapshot,
    ) -> Vec<AgentEffect> {
        if self.inspector_run_id.as_deref() == Some(snapshot.run.run_id.as_str()) {
            self.inspector_run_id = None;
        }
        if !snapshot.run.lease.owned_by_client {
            return Vec::new();
        }
        vec![AgentEffect::Detach {
            run_id: snapshot.run.run_id.clone(),
            lease_generation: snapshot.run.lease.generation,
            action_request_id: self.next_action_id(),
        }]
    }

    /// Apply the foreground-priority permission gate at arrival time. Inline
    /// transient focus is released without discarding target-bound drafts;
    /// an attached inspector remains attached but loses any highlighted
    /// background option so buffered input cannot act across sessions.
    pub fn foreground_permission_arrived(&mut self) -> Vec<AgentEffect> {
        if let AgentSurface::Inspector(inspector) = &mut self.surface {
            inspector.permission_focused = false;
            inspector.permission_selected = None;
            inspector.permission_scroll = 0;
            return Vec::new();
        }
        let inline_permission = matches!(self.surface, AgentSurface::Permission { .. });
        if let Some(pending) = &mut self.pending {
            if inline_permission {
                self.surface = AgentSurface::Peek;
            }
            if pending.stage == PendingStage::LeaseAcquisition {
                pending.abandoned = true;
                if pending.intent == Some(AgentLeaseIntent::Attach) {
                    self.inspector_run_id = None;
                }
            }
            return Vec::new();
        }
        let run_id = match &self.surface {
            AgentSurface::Permission { run_id, .. } | AgentSurface::Reply { run_id } => {
                Some(run_id.clone())
            }
            _ => None,
        };
        let release = run_id.and_then(|run_id| {
            self.run_by_id(&run_id)
                .filter(|run| run.lease.owned_by_client)
                .map(|run| (run_id.clone(), run.lease.generation))
        });
        if matches!(self.surface, AgentSurface::Permission { .. }) {
            self.surface = AgentSurface::Peek;
        }
        let Some((run_id, lease_generation)) = release else {
            return Vec::new();
        };
        if let Some(run) = self
            .snapshot
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
        {
            run.lease = AgentLeaseView::default();
        }
        vec![AgentEffect::ReleaseLease {
            run_id,
            lease_generation,
            action_request_id: self.next_action_id(),
        }]
    }

    /// Apply one live retained event. A gap is never merged; it requests a
    /// fresh atomic snapshot and leaves the visible history unchanged.
    pub fn apply_history_event(&mut self, event: AgentHistoryEvent) -> Vec<AgentEffect> {
        let AgentSurface::Inspector(inspector) = &mut self.surface else {
            return Vec::new();
        };
        if event.seq <= inspector.snapshot_seq {
            return Vec::new();
        }
        let expected = inspector.snapshot_seq.saturating_add(1);
        if event.seq != expected {
            return vec![AgentEffect::Resync {
                run_id: inspector.run.run_id.clone(),
                expected_seq: expected,
                received_seq: event.seq,
            }];
        }
        let follow_end = inspector.scroll >= inspector.last_content_line();
        inspector.snapshot_seq = event.seq;
        inspector.events.push(event);
        if follow_end {
            inspector.scroll = inspector.last_content_line();
        }
        Vec::new()
    }

    /// Open/collapse the inline command center. Standalone mode is already a
    /// manager and therefore stays expanded.
    pub fn toggle(&mut self) {
        match self.surface {
            AgentSurface::Collapsed => {
                self.surface = AgentSurface::List;
                self.ensure_selection();
            }
            AgentSurface::Inspector(_) => {}
            _ if self.mode == AgentDeckMode::Inline => {
                self.surface = AgentSurface::Collapsed;
                self.filtering = false;
            }
            _ => {}
        }
    }

    /// Reduce one event/acknowledgement. Foreground permission state is an
    /// explicit argument from Code and disables every background mutation.
    pub fn step(
        &mut self,
        action: AgentAction,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        match action {
            AgentAction::Event(event) => self.event(event, foreground_permission_pending),
            #[cfg(test)]
            AgentAction::TestLegacyKey(code) => {
                let command = match (&self.surface, code) {
                    (AgentSurface::Collapsed, KeyCode::F(5)) => Some(AgentDeckCommand::Open),
                    (AgentSurface::List, KeyCode::F(5)) => Some(AgentDeckCommand::Detach),
                    (AgentSurface::List, KeyCode::Char('x' | 'X')) => {
                        Some(AgentDeckCommand::ToggleStopped)
                    }
                    (AgentSurface::List, KeyCode::Char('/')) => Some(AgentDeckCommand::Filter),
                    (AgentSurface::List, KeyCode::Char(' ')) => Some(AgentDeckCommand::Peek),
                    (AgentSurface::List | AgentSurface::Peek, KeyCode::Char('r' | 'R')) => {
                        Some(AgentDeckCommand::Reply)
                    }
                    (AgentSurface::List, KeyCode::Char('n' | 'N')) => Some(AgentDeckCommand::New),
                    (AgentSurface::List | AgentSurface::Peek, KeyCode::Char('c' | 'C')) => {
                        Some(AgentDeckCommand::Cancel)
                    }
                    (AgentSurface::List | AgentSurface::Peek, KeyCode::Char('m' | 'M')) => {
                        Some(AgentDeckCommand::MarkReviewed)
                    }
                    (AgentSurface::List | AgentSurface::Peek, KeyCode::Char('s' | 'S')) => {
                        Some(AgentDeckCommand::Stop)
                    }
                    (AgentSurface::List | AgentSurface::Peek, KeyCode::Char('t' | 'T')) => {
                        Some(AgentDeckCommand::Takeover)
                    }
                    (AgentSurface::Peek, KeyCode::F(2)) => Some(AgentDeckCommand::ReviewPermission),
                    (AgentSurface::Inspector(_), KeyCode::F(2)) => {
                        Some(AgentDeckCommand::ReviewPermission)
                    }
                    (AgentSurface::Inspector(_), KeyCode::F(5)) => Some(AgentDeckCommand::Detach),
                    _ => None,
                };
                if let Some(command) = command {
                    self.command(command, foreground_permission_pending)
                } else {
                    self.event(
                        Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                        foreground_permission_pending,
                    )
                }
            }
            AgentAction::LeaseAcquired {
                run_id,
                intent,
                generation,
                action_request_id,
            } => self.lease_acquired(
                run_id,
                intent,
                generation,
                action_request_id,
                foreground_permission_pending,
            ),
            AgentAction::MutationAccepted { action_request_id } => {
                self.mutation_accepted(&action_request_id)
            }
            AgentAction::EffectFailed {
                action_request_id,
                message,
            } => {
                let failed = self
                    .pending
                    .as_ref()
                    .filter(|pending| pending.id == action_request_id);
                let failed_run_id = failed.and_then(|pending| pending.run_id.clone());
                let failed_permission = failed
                    .is_some_and(|pending| pending.intent == Some(AgentLeaseIntent::Permission));
                let failed_transient = failed.is_some_and(|pending| {
                    matches!(
                        pending.intent,
                        Some(
                            AgentLeaseIntent::Reply
                                | AgentLeaseIntent::Permission
                                | AgentLeaseIntent::Cancel
                                | AgentLeaseIntent::MarkReviewed
                                | AgentLeaseIntent::Stop
                                | AgentLeaseIntent::Attach
                        )
                    )
                });
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == action_request_id)
                {
                    self.pending = None;
                }
                if !matches!(self.surface, AgentSurface::Inspector(_))
                    && failed_run_id.as_deref() == self.inspector_run_id.as_deref()
                {
                    self.inspector_run_id = None;
                }
                if failed_transient
                    && !matches!(self.surface, AgentSurface::Inspector(_))
                    && let Some(run_id) = &failed_run_id
                    && let Some(run) = self
                        .snapshot
                        .runs
                        .iter_mut()
                        .find(|run| run.run_id == *run_id)
                {
                    run.lease = AgentLeaseView::default();
                }
                if failed_permission && !matches!(self.surface, AgentSurface::Inspector(_)) {
                    self.surface = AgentSurface::Peek;
                }
                self.notice = Some(message);
                Vec::new()
            }
        }
    }

    fn event(&mut self, event: Event, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.pending.is_some() {
            if let Event::Paste(text) = &event
                && self.pending_allows_reply_editing()
                && let AgentSurface::Reply { run_id } = &self.surface
            {
                self.reply_drafts
                    .entry(run_id.clone())
                    .or_default()
                    .paste(text);
                return Vec::new();
            }
            let Some(key) = pressed(&event) else {
                return Vec::new();
            };
            return self.pending_key(*key);
        }
        if let Event::Paste(text) = &event {
            match &self.surface {
                AgentSurface::Reply { run_id } => {
                    self.reply_drafts
                        .entry(run_id.clone())
                        .or_default()
                        .paste(text);
                }
                AgentSurface::NewRun => {
                    if self.new_run_focus == NewRunFocus::Prompt {
                        self.new_run_draft.paste(text);
                    } else {
                        self.paste_new_run_target(text);
                    }
                }
                _ => {}
            }
            return Vec::new();
        }
        let Some(key) = pressed(&event) else {
            return Vec::new();
        };
        if key.code == KeyCode::F(5) && self.mode == AgentDeckMode::Standalone {
            return self.f5();
        }
        if matches!(self.surface, AgentSurface::Inspector(_)) {
            return self.inspector_key(*key, foreground_permission_pending);
        }
        if self.filtering {
            return self.filter_key(*key);
        }
        match self.surface.clone() {
            AgentSurface::Collapsed => Vec::new(),
            AgentSurface::List => self.list_key(*key, foreground_permission_pending),
            AgentSurface::Peek => self.peek_key(*key, foreground_permission_pending),
            AgentSurface::Reply { run_id } => {
                self.reply_key(*key, &run_id, foreground_permission_pending)
            }
            AgentSurface::NewRun => self.new_run_key(*key, foreground_permission_pending),
            AgentSurface::Permission {
                run_id,
                permission_id,
                lease_generation,
                selected,
            } => self.permission_key(
                *key,
                &run_id,
                &permission_id,
                lease_generation,
                selected,
                foreground_permission_pending,
            ),
            AgentSurface::Confirm { run_id, intent } => {
                self.confirm_key(*key, &run_id, intent, foreground_permission_pending)
            }
            AgentSurface::TakeoverConfirm { run_id, selected } => {
                self.takeover_key(*key, &run_id, selected, foreground_permission_pending)
            }
            AgentSurface::ExitConfirm { selected } => self.exit_confirm_key(*key, selected),
            AgentSurface::Inspector(_) => Vec::new(),
        }
    }

    fn f5(&mut self) -> Vec<AgentEffect> {
        if let AgentSurface::Inspector(inspector) = &self.surface {
            let effect = AgentEffect::Detach {
                run_id: inspector.run.run_id.clone(),
                lease_generation: inspector.run.lease.generation,
                action_request_id: self.next_action_id(),
            };
            self.inspector_run_id = None;
            self.surface = AgentSurface::List;
            return vec![effect];
        }
        let transient_run_id = if self.mode == AgentDeckMode::Inline {
            match &self.surface {
                AgentSurface::Reply { run_id } | AgentSurface::Permission { run_id, .. } => {
                    Some(run_id.clone())
                }
                _ => None,
            }
        } else {
            None
        };
        self.toggle();
        transient_run_id.map_or_else(Vec::new, |run_id| self.release_inline_lease(&run_id))
    }

    fn list_key(&mut self, key: KeyEvent, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.mode == AgentDeckMode::Inline
            && matches!(
                key.code,
                KeyCode::Char(
                    '/' | 'x'
                        | 'X'
                        | ' '
                        | 'r'
                        | 'R'
                        | 'n'
                        | 'N'
                        | 'c'
                        | 'C'
                        | 'm'
                        | 'M'
                        | 's'
                        | 'S'
                        | 't'
                        | 'T'
                )
            )
        {
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.mode == AgentDeckMode::Standalone =>
            {
                return self.request_standalone_exit();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('x') | KeyCode::Char('X') => {
                self.show_stopped = !self.show_stopped;
                self.ensure_selection();
            }
            KeyCode::Char(' ') => {
                if self.selected_run().is_some() {
                    self.surface = AgentSurface::Peek;
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                return self.begin_reply(foreground_permission_pending);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.mutations_blocked(foreground_permission_pending) {
                    return Vec::new();
                }
                let target = self
                    .default_new_run_target()
                    .or_else(|| self.snapshot.new_run_target.clone())
                    .unwrap_or_else(|| NewAgentRunTarget {
                        agent: String::new(),
                        directory: String::new(),
                        route: None,
                        conflict: None,
                    });
                self.set_new_run_target(target);
                self.new_run_focus = NewRunFocus::Prompt;
                self.surface = AgentSurface::NewRun;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                return self
                    .begin_confirmation(AgentLeaseIntent::Cancel, foreground_permission_pending);
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                return self.begin_confirmation(
                    AgentLeaseIntent::MarkReviewed,
                    foreground_permission_pending,
                );
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                return self
                    .begin_confirmation(AgentLeaseIntent::Stop, foreground_permission_pending);
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.begin_takeover(foreground_permission_pending);
            }
            KeyCode::Enter => return self.begin_attach(foreground_permission_pending),
            KeyCode::Esc if self.mode == AgentDeckMode::Inline => self.toggle(),
            KeyCode::Esc if self.mode == AgentDeckMode::Standalone => {
                return self.request_standalone_exit();
            }
            _ => {}
        }
        Vec::new()
    }

    fn peek_key(&mut self, key: KeyEvent, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.mode == AgentDeckMode::Inline
            && matches!(
                key.code,
                KeyCode::Char(' ' | 'r' | 'R' | 'c' | 'C' | 'm' | 'M' | 's' | 'S' | 't' | 'T')
                    | KeyCode::F(2)
            )
        {
            return Vec::new();
        }
        match key.code {
            KeyCode::Char(' ') | KeyCode::Esc => self.surface = AgentSurface::List,
            KeyCode::Char('r') | KeyCode::Char('R') => {
                return self.begin_reply(foreground_permission_pending);
            }
            KeyCode::F(2) => return self.begin_permission(foreground_permission_pending),
            KeyCode::Enter => return self.begin_attach(foreground_permission_pending),
            KeyCode::Char('c') | KeyCode::Char('C') => {
                return self
                    .begin_confirmation(AgentLeaseIntent::Cancel, foreground_permission_pending);
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                return self.begin_confirmation(
                    AgentLeaseIntent::MarkReviewed,
                    foreground_permission_pending,
                );
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                return self
                    .begin_confirmation(AgentLeaseIntent::Stop, foreground_permission_pending);
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.begin_takeover(foreground_permission_pending);
            }
            _ => {}
        }
        Vec::new()
    }

    fn reply_key(
        &mut self,
        key: KeyEvent,
        run_id: &str,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if foreground_permission_pending {
            self.notice = Some(
                "Resolve the foreground permission before mutating a background run".to_string(),
            );
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            self.surface = AgentSurface::Peek;
            return self.release_inline_lease(run_id);
        }
        let edit = self
            .reply_drafts
            .entry(run_id.to_string())
            .or_default()
            .apply(key);
        if edit != Edit::Submitted {
            return Vec::new();
        }
        let Some(run) = self.run_by_id(run_id).cloned() else {
            self.notice = Some("That background run is no longer available".to_string());
            return Vec::new();
        };
        let prompt = self
            .reply_drafts
            .get(run_id)
            .map(|editor| editor.text().to_string())
            .unwrap_or_default();
        if prompt.trim().is_empty() {
            self.notice = Some("Reply draft is empty".to_string());
            return Vec::new();
        }
        if !run.lease.owned_by_client {
            if let Some(owner) = &run.lease.owner {
                self.notice = Some(format!("Lease lost · controlled by {owner}"));
                return Vec::new();
            }
            return self.acquire(run_id, AgentLeaseIntent::Reply);
        }
        let id = self.next_action_id();
        self.pending = Some(PendingAction {
            id: id.clone(),
            run_id: Some(run_id.to_string()),
            intent: Some(AgentLeaseIntent::Reply),
            stage: PendingStage::Mutation,
            abandoned: false,
            clears_reply: true,
            clears_new_run: false,
        });
        vec![AgentEffect::Reply {
            run_id: run_id.to_string(),
            prompt,
            lease_generation: run.lease.generation,
            action_request_id: id,
        }]
    }

    fn new_run_key(
        &mut self,
        key: KeyEvent,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if key.code == KeyCode::Esc {
            self.surface = AgentSurface::List;
            return Vec::new();
        }
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.new_run_focus = self
                .new_run_focus
                .next(key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT));
            return Vec::new();
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down)
            && self.new_run_focus != NewRunFocus::Prompt
        {
            self.cycle_new_run_target(key.code == KeyCode::Up);
            return Vec::new();
        }
        if self.new_run_focus != NewRunFocus::Prompt {
            if key.code == KeyCode::Enter {
                self.new_run_focus = self.new_run_focus.next(false);
                return Vec::new();
            }
            self.edit_new_run_target(key);
            return Vec::new();
        }
        let edit = self.new_run_draft.apply(key);
        if edit != Edit::Submitted {
            return Vec::new();
        }
        let Some(target) = self.new_run_target.clone() else {
            self.notice = Some("Choose a safe agent and directory before dispatch".to_string());
            return Vec::new();
        };
        if target.agent.trim().is_empty() || target.directory.trim().is_empty() {
            self.notice = Some("Agent and canonical directory are required".to_string());
            return Vec::new();
        }
        if let Some(conflict) = &target.conflict {
            self.notice = Some(format!("Directory claim blocked: {conflict}"));
            return Vec::new();
        }
        let prompt = self.new_run_draft.text().to_string();
        if prompt.trim().is_empty() {
            self.notice = Some("New background prompt is empty".to_string());
            return Vec::new();
        }
        let id = self.next_action_id();
        self.pending = Some(PendingAction {
            id: id.clone(),
            run_id: None,
            intent: None,
            stage: PendingStage::Mutation,
            abandoned: false,
            clears_reply: false,
            clears_new_run: true,
        });
        vec![AgentEffect::NewRun {
            target,
            prompt,
            action_request_id: id,
        }]
    }

    fn permission_key(
        &mut self,
        key: KeyEvent,
        run_id: &str,
        permission_id: &str,
        lease_generation: u64,
        selected: Option<usize>,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if foreground_permission_pending {
            self.surface = AgentSurface::Peek;
            self.notice = Some("Foreground permission waiting · / to review it".to_string());
            return Vec::new();
        }
        let Some(permission) = self.permission_for(run_id, permission_id).cloned() else {
            self.surface = AgentSurface::Peek;
            self.notice = Some("That permission is no longer pending".to_string());
            return Vec::new();
        };
        let mut next = selected;
        match key.code {
            KeyCode::Esc => {
                self.surface = AgentSurface::Peek;
                return self.release_inline_lease(run_id);
            }
            KeyCode::Up => next = previous_selection(next, permission.options.len()),
            KeyCode::Down => next = next_selection(next, permission.options.len()),
            KeyCode::Char(value @ '1'..='9') => {
                next = value
                    .to_digit(10)
                    .and_then(|digit| usize::try_from(digit.saturating_sub(1)).ok())
                    .filter(|index| *index < permission.options.len());
            }
            KeyCode::Enter => {
                let Some(index) = next else {
                    return Vec::new();
                };
                let Some(option) = permission.options.get(index) else {
                    return Vec::new();
                };
                let id = self.next_action_id();
                self.pending = Some(PendingAction {
                    id: id.clone(),
                    run_id: Some(run_id.to_string()),
                    intent: Some(AgentLeaseIntent::Permission),
                    stage: PendingStage::Mutation,
                    abandoned: false,
                    clears_reply: false,
                    clears_new_run: false,
                });
                return vec![AgentEffect::RespondPermission {
                    run_id: run_id.to_string(),
                    permission_id: permission_id.to_string(),
                    option_id: option.id.clone(),
                    lease_generation,
                    action_request_id: id,
                }];
            }
            _ => {}
        }
        self.surface = AgentSurface::Permission {
            run_id: run_id.to_string(),
            permission_id: permission_id.to_string(),
            lease_generation,
            selected: next,
        };
        Vec::new()
    }

    fn confirm_key(
        &mut self,
        key: KeyEvent,
        run_id: &str,
        intent: AgentLeaseIntent,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if key.code == KeyCode::Esc {
            self.surface = AgentSurface::Peek;
            return Vec::new();
        }
        if key.code != KeyCode::Enter || self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run) = self.run_by_id(run_id).cloned() else {
            return Vec::new();
        };
        if !run.lease.owned_by_client {
            return self.acquire(run_id, intent);
        }
        let id = self.next_action_id();
        self.pending = Some(PendingAction {
            id: id.clone(),
            run_id: Some(run_id.to_string()),
            intent: Some(intent),
            stage: PendingStage::Mutation,
            abandoned: false,
            clears_reply: false,
            clears_new_run: false,
        });
        let effect = match intent {
            AgentLeaseIntent::Cancel => AgentEffect::CancelTurn {
                run_id: run_id.to_string(),
                lease_generation: run.lease.generation,
                action_request_id: id,
            },
            AgentLeaseIntent::MarkReviewed => AgentEffect::MarkReviewed {
                run_id: run_id.to_string(),
                lease_generation: run.lease.generation,
                action_request_id: id,
            },
            AgentLeaseIntent::Stop => AgentEffect::Stop {
                run_id: run_id.to_string(),
                lease_generation: run.lease.generation,
                action_request_id: id,
            },
            AgentLeaseIntent::Reply | AgentLeaseIntent::Permission | AgentLeaseIntent::Attach => {
                return Vec::new();
            }
        };
        vec![effect]
    }

    fn begin_takeover(&mut self, foreground_permission_pending: bool) {
        if self.mutations_blocked(foreground_permission_pending) {
            return;
        }
        let Some(run) = self.selected_run() else {
            return;
        };
        if run.lease.owner.is_none() || run.lease.owned_by_client {
            self.notice =
                Some("Takeover is only available when another client owns control".to_string());
            return;
        }
        self.surface = AgentSurface::TakeoverConfirm {
            run_id: run.run_id.clone(),
            selected: false,
        };
    }

    fn takeover_key(
        &mut self,
        key: KeyEvent,
        run_id: &str,
        selected: bool,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if key.code == KeyCode::Esc {
            self.surface = AgentSurface::Peek;
            return Vec::new();
        }
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run) = self.run_by_id(run_id) else {
            self.surface = AgentSurface::List;
            self.notice = Some("That background run is no longer available".to_string());
            return Vec::new();
        };
        if run.lease.owner.is_none() || run.lease.owned_by_client {
            self.surface = AgentSurface::Peek;
            self.notice = Some("Control ownership changed · takeover was not sent".to_string());
            return Vec::new();
        }
        let selected = match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => !selected,
            KeyCode::Enter if selected => {
                let id = self.next_action_id();
                self.inspector_run_id = Some(run_id.to_string());
                self.pending = Some(PendingAction {
                    id: id.clone(),
                    run_id: Some(run_id.to_string()),
                    intent: None,
                    stage: PendingStage::Mutation,
                    abandoned: false,
                    clears_reply: false,
                    clears_new_run: false,
                });
                return vec![AgentEffect::Takeover {
                    run_id: run_id.to_string(),
                    action_request_id: id,
                }];
            }
            _ => selected,
        };
        self.surface = AgentSurface::TakeoverConfirm {
            run_id: run_id.to_string(),
            selected,
        };
        Vec::new()
    }

    fn request_standalone_exit(&mut self) -> Vec<AgentEffect> {
        if !self.has_unsent_drafts() {
            return vec![AgentEffect::ExitStandalone];
        }
        self.surface = AgentSurface::ExitConfirm { selected: None };
        Vec::new()
    }

    fn exit_confirm_key(&mut self, key: KeyEvent, selected: Option<usize>) -> Vec<AgentEffect> {
        if key.code == KeyCode::Esc {
            self.surface = AgentSurface::List;
            return Vec::new();
        }
        let selected = match key.code {
            KeyCode::Up => previous_selection(selected, 2),
            KeyCode::Down => next_selection(selected, 2),
            KeyCode::Char('1') => Some(0),
            KeyCode::Char('2') => Some(1),
            KeyCode::Enter => match selected {
                Some(0) => {
                    self.surface = AgentSurface::List;
                    return Vec::new();
                }
                Some(1) => {
                    self.reply_drafts.clear();
                    self.new_run_draft.clear();
                    return vec![AgentEffect::ExitStandalone];
                }
                _ => return Vec::new(),
            },
            _ => selected,
        };
        self.surface = AgentSurface::ExitConfirm { selected };
        Vec::new()
    }

    fn inspector_key(
        &mut self,
        key: KeyEvent,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        let standalone = self.mode == AgentDeckMode::Standalone;
        let AgentSurface::Inspector(inspector) = &mut self.surface else {
            return Vec::new();
        };
        if foreground_permission_pending && inspector.permission_focused {
            inspector.permission_focused = false;
            inspector.permission_selected = None;
            inspector.permission_scroll = 0;
            self.notice = Some(
                "Foreground permission waiting · background selection cleared · / to return"
                    .to_string(),
            );
            return Vec::new();
        }
        if inspector.permission_focused {
            let AgentAttention::Permission(permission) = &inspector.run.attention else {
                inspector.permission_focused = false;
                inspector.permission_selected = None;
                return Vec::new();
            };
            match key.code {
                KeyCode::Esc => {
                    inspector.permission_focused = false;
                    inspector.permission_selected = None;
                    inspector.permission_scroll = 0;
                }
                KeyCode::Up => {
                    inspector.permission_selected =
                        previous_selection(inspector.permission_selected, permission.options.len());
                    inspector.permission_scroll = 0;
                }
                KeyCode::Down => {
                    inspector.permission_selected =
                        next_selection(inspector.permission_selected, permission.options.len());
                    inspector.permission_scroll = 0;
                }
                KeyCode::Char(value @ '1'..='9') => {
                    inspector.permission_selected = value
                        .to_digit(10)
                        .and_then(|digit| usize::try_from(digit.saturating_sub(1)).ok())
                        .filter(|index| *index < permission.options.len());
                    inspector.permission_scroll = 0;
                }
                KeyCode::PageUp => {
                    inspector.permission_scroll = inspector.permission_scroll.saturating_sub(8);
                }
                KeyCode::PageDown => {
                    inspector.permission_scroll = inspector.permission_scroll.saturating_add(8);
                }
                KeyCode::Home => inspector.permission_scroll = 0,
                KeyCode::End => inspector.permission_scroll = usize::MAX,
                KeyCode::Enter => {
                    let Some(index) = inspector.permission_selected else {
                        return Vec::new();
                    };
                    let Some(option) = permission.options.get(index) else {
                        return Vec::new();
                    };
                    self.next_action = self.next_action.saturating_add(1);
                    let id = format!("{}:{}", self.client_id, self.next_action);
                    self.pending = Some(PendingAction {
                        id: id.clone(),
                        run_id: Some(inspector.run.run_id.clone()),
                        intent: Some(AgentLeaseIntent::Permission),
                        stage: PendingStage::Mutation,
                        abandoned: false,
                        clears_reply: false,
                        clears_new_run: false,
                    });
                    return vec![AgentEffect::RespondPermission {
                        run_id: inspector.run.run_id.clone(),
                        permission_id: permission.permission_id.clone(),
                        option_id: option.id.clone(),
                        lease_generation: inspector.run.lease.generation,
                        action_request_id: id,
                    }];
                }
                _ => {}
            }
            return Vec::new();
        }
        if inspector.searching {
            match key.code {
                KeyCode::Esc => inspector.searching = false,
                KeyCode::Backspace => {
                    inspector.search.pop();
                }
                KeyCode::Enter => {
                    inspector.searching = false;
                    if let Some(index) =
                        first_history_match(&inspector.content(), &inspector.search)
                    {
                        inspector.scroll = index;
                    }
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    inspector.search.push(character);
                }
                _ => {}
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Up => inspector.scroll = inspector.scroll.saturating_sub(1),
            KeyCode::Down => {
                inspector.scroll = inspector
                    .scroll
                    .saturating_add(1)
                    .min(inspector.last_content_line());
            }
            KeyCode::PageUp => inspector.scroll = inspector.scroll.saturating_sub(8),
            KeyCode::PageDown => {
                inspector.scroll = inspector
                    .scroll
                    .saturating_add(8)
                    .min(inspector.last_content_line());
            }
            KeyCode::Home => inspector.scroll = 0,
            KeyCode::End => inspector.scroll = inspector.last_content_line(),
            KeyCode::Char('f') if standalone && key.modifiers.contains(KeyModifiers::CONTROL) => {
                inspector.searching = true;
            }
            KeyCode::Char('y') if standalone && key.modifiers.contains(KeyModifiers::CONTROL) => {
                return vec![AgentEffect::Copy {
                    text: inspector.content(),
                }];
            }
            KeyCode::Char('e') if standalone && key.modifiers.contains(KeyModifiers::CONTROL) => {
                return vec![AgentEffect::Export {
                    run_id: inspector.run.run_id.clone(),
                    content: inspector.content(),
                }];
            }
            KeyCode::F(2) if standalone && foreground_permission_pending => {
                self.notice = Some("Foreground permission waiting · / to return".to_string());
            }
            KeyCode::F(2)
                if standalone
                    && matches!(inspector.run.attention, AgentAttention::Permission(_)) =>
            {
                inspector.permission_focused = true;
                inspector.permission_selected = None;
                inspector.permission_scroll = 0;
            }
            KeyCode::F(5) if standalone => return self.f5(),
            _ => {}
        }
        Vec::new()
    }

    fn pending_allows_reply_editing(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.stage == PendingStage::LeaseAcquisition
                && !pending.abandoned
                && pending.intent == Some(AgentLeaseIntent::Reply)
        })
    }

    fn pending_key(&mut self, key: KeyEvent) -> Vec<AgentEffect> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending.stage == PendingStage::Mutation {
            self.notice =
                Some("Background action in progress · waiting for supervisor".to_string());
            return Vec::new();
        }
        if pending.abandoned {
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            self.abandon_pending_acquisition(false);
            return Vec::new();
        }
        if self.pending_allows_reply_editing() {
            if key.code == KeyCode::Enter {
                self.notice = Some("Waiting for control before sending this reply".to_string());
                return Vec::new();
            }
            if let AgentSurface::Reply { run_id } = &self.surface {
                let run_id = run_id.clone();
                let _ = self.reply_drafts.entry(run_id).or_default().apply(key);
            }
            return Vec::new();
        }
        self.notice = Some("Waiting for the background control lease".to_string());
        Vec::new()
    }

    fn abandon_pending_acquisition(&mut self, collapse: bool) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if pending.stage != PendingStage::LeaseAcquisition {
            return;
        }
        pending.abandoned = true;
        if pending.intent == Some(AgentLeaseIntent::Attach) {
            self.inspector_run_id = None;
        }
        let next_surface = if collapse && self.mode == AgentDeckMode::Inline {
            AgentSurface::Collapsed
        } else if matches!(
            self.surface,
            AgentSurface::Reply { .. }
                | AgentSurface::Permission { .. }
                | AgentSurface::Confirm { .. }
        ) {
            AgentSurface::Peek
        } else {
            self.surface.clone()
        };
        self.surface = next_surface;
        self.notice =
            Some("Control request cancelled · any late lease will be released".to_string());
    }

    fn filter_key(&mut self, key: KeyEvent) -> Vec<AgentEffect> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
                self.ensure_selection();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(character);
                self.ensure_selection();
            }
            _ => {}
        }
        Vec::new()
    }

    fn begin_reply(&mut self, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run) = self.selected_run().cloned() else {
            return Vec::new();
        };
        if matches!(
            run.process,
            AgentProcessState::Stopped | AgentProcessState::Interrupted
        ) {
            self.notice = Some("Stopped or interrupted runs cannot receive replies".to_string());
            return Vec::new();
        }
        if run.lease.owner.is_some() && !run.lease.owned_by_client {
            self.notice = Some(format!(
                "Read only · controlled by {}",
                run.lease.owner.as_deref().unwrap_or("another client")
            ));
            return Vec::new();
        }
        self.surface = AgentSurface::Reply {
            run_id: run.run_id.clone(),
        };
        if run.lease.owned_by_client {
            Vec::new()
        } else {
            self.acquire(&run.run_id, AgentLeaseIntent::Reply)
        }
    }

    fn begin_permission(&mut self, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run) = self.selected_run().cloned() else {
            return Vec::new();
        };
        let AgentAttention::Permission(permission) = &run.attention else {
            return Vec::new();
        };
        if permission_needs_inspector(permission) {
            return self.begin_attach(foreground_permission_pending);
        }
        if run.lease.owner.is_some() && !run.lease.owned_by_client {
            self.notice = Some(format!(
                "Read only · controlled by {}",
                run.lease.owner.as_deref().unwrap_or("another client")
            ));
            return Vec::new();
        }
        if run.lease.owned_by_client {
            self.surface = AgentSurface::Permission {
                run_id: run.run_id,
                permission_id: permission.permission_id.clone(),
                lease_generation: run.lease.generation,
                selected: None,
            };
            Vec::new()
        } else {
            self.acquire(&run.run_id, AgentLeaseIntent::Permission)
        }
    }

    fn begin_confirmation(
        &mut self,
        intent: AgentLeaseIntent,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run_id) = self.selected_run_id.clone() else {
            return Vec::new();
        };
        self.surface = AgentSurface::Confirm { run_id, intent };
        Vec::new()
    }

    fn begin_attach(&mut self, foreground_permission_pending: bool) -> Vec<AgentEffect> {
        if self.mutations_blocked(foreground_permission_pending) {
            return Vec::new();
        }
        let Some(run) = self.selected_run().cloned() else {
            return Vec::new();
        };
        if run.lease.owner.is_some() && !run.lease.owned_by_client {
            self.notice = Some(format!(
                "Read only · controlled by {}",
                run.lease.owner.as_deref().unwrap_or("another client")
            ));
            return Vec::new();
        }
        self.inspector_run_id = Some(run.run_id.clone());
        let id = self.next_action_id();
        self.pending = Some(PendingAction {
            id: id.clone(),
            run_id: Some(run.run_id.clone()),
            intent: Some(AgentLeaseIntent::Attach),
            stage: if run.lease.owned_by_client {
                PendingStage::Mutation
            } else {
                PendingStage::LeaseAcquisition
            },
            abandoned: false,
            clears_reply: false,
            clears_new_run: false,
        });
        if run.lease.owned_by_client {
            vec![AgentEffect::Attach {
                run_id: run.run_id,
                action_request_id: id,
            }]
        } else {
            vec![AgentEffect::AcquireLease {
                run_id: run.run_id,
                intent: AgentLeaseIntent::Attach,
                action_request_id: id,
            }]
        }
    }

    fn acquire(&mut self, run_id: &str, intent: AgentLeaseIntent) -> Vec<AgentEffect> {
        let id = self.next_action_id();
        self.pending = Some(PendingAction {
            id: id.clone(),
            run_id: Some(run_id.to_string()),
            intent: Some(intent),
            stage: PendingStage::LeaseAcquisition,
            abandoned: false,
            clears_reply: false,
            clears_new_run: false,
        });
        vec![AgentEffect::AcquireLease {
            run_id: run_id.to_string(),
            intent,
            action_request_id: id,
        }]
    }

    fn lease_acquired(
        &mut self,
        run_id: String,
        intent: AgentLeaseIntent,
        generation: u64,
        action_request_id: String,
        foreground_permission_pending: bool,
    ) -> Vec<AgentEffect> {
        let matches_pending = self.pending.as_ref().is_some_and(|pending| {
            pending.id == action_request_id
                && pending.run_id.as_deref() == Some(run_id.as_str())
                && pending.intent == Some(intent)
                && pending.stage == PendingStage::LeaseAcquisition
        });
        if !matches_pending {
            return Vec::new();
        }
        let abandoned = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.abandoned);
        if let Some(run) = self
            .snapshot
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
        {
            run.lease.generation = generation;
            run.lease.owned_by_client = true;
            run.lease.owner = Some(self.client_id.clone());
        }
        self.pending = None;
        if abandoned || foreground_permission_pending {
            if intent == AgentLeaseIntent::Attach {
                self.inspector_run_id = None;
            }
            if foreground_permission_pending && !matches!(intent, AgentLeaseIntent::Reply) {
                self.surface = AgentSurface::Peek;
            }
            if let Some(run) = self
                .snapshot
                .runs
                .iter_mut()
                .find(|run| run.run_id == run_id)
            {
                run.lease = AgentLeaseView::default();
            }
            self.notice = Some(if foreground_permission_pending {
                "Foreground permission arrived · background lease released without acting"
                    .to_string()
            } else {
                "Control request was cancelled · late lease released without acting".to_string()
            });
            return vec![AgentEffect::ReleaseLease {
                run_id,
                lease_generation: generation,
                action_request_id: self.next_action_id(),
            }];
        }
        match intent {
            AgentLeaseIntent::Permission => {
                let permission_id = self.run_by_id(&run_id).and_then(|run| {
                    if let AgentAttention::Permission(permission) = &run.attention {
                        Some(permission.permission_id.clone())
                    } else {
                        None
                    }
                });
                if let Some(permission_id) = permission_id {
                    self.surface = AgentSurface::Permission {
                        run_id,
                        permission_id,
                        lease_generation: generation,
                        selected: None,
                    };
                }
                Vec::new()
            }
            AgentLeaseIntent::Attach => {
                let id = self.next_action_id();
                self.pending = Some(PendingAction {
                    id: id.clone(),
                    run_id: Some(run_id.clone()),
                    intent: Some(AgentLeaseIntent::Attach),
                    stage: PendingStage::Mutation,
                    abandoned: false,
                    clears_reply: false,
                    clears_new_run: false,
                });
                vec![AgentEffect::Attach {
                    run_id,
                    action_request_id: id,
                }]
            }
            AgentLeaseIntent::Cancel | AgentLeaseIntent::MarkReviewed | AgentLeaseIntent::Stop => {
                self.confirm_key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &run_id,
                    intent,
                    false,
                )
            }
            AgentLeaseIntent::Reply => Vec::new(),
        }
    }

    fn mutation_accepted(&mut self, action_request_id: &str) -> Vec<AgentEffect> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        if pending.id != action_request_id {
            self.pending = Some(pending);
            return Vec::new();
        }
        let attached = matches!(self.surface, AgentSurface::Inspector(_));
        if pending.clears_reply {
            if let Some(run_id) = &pending.run_id
                && let Some(editor) = self.reply_drafts.get_mut(run_id)
            {
                editor.clear();
            }
            self.surface = AgentSurface::List;
        }
        if pending.clears_new_run {
            self.new_run_draft.clear();
            self.surface = AgentSurface::List;
        }
        if matches!(pending.intent, Some(AgentLeaseIntent::Permission)) {
            if let AgentSurface::Inspector(inspector) = &mut self.surface {
                inspector.permission_focused = false;
                inspector.permission_selected = None;
                inspector.permission_scroll = 0;
            } else {
                self.surface = AgentSurface::Peek;
            }
        }
        if matches!(
            pending.intent,
            Some(
                AgentLeaseIntent::Cancel | AgentLeaseIntent::MarkReviewed | AgentLeaseIntent::Stop
            )
        ) {
            self.surface = AgentSurface::List;
        }
        if !attached
            && matches!(
                pending.intent,
                Some(
                    AgentLeaseIntent::Reply
                        | AgentLeaseIntent::Permission
                        | AgentLeaseIntent::Cancel
                        | AgentLeaseIntent::MarkReviewed
                        | AgentLeaseIntent::Stop
                )
            )
            && let Some(run_id) = pending.run_id
            && let Some(run) = self
                .snapshot
                .runs
                .iter_mut()
                .find(|run| run.run_id == run_id)
        {
            run.lease = AgentLeaseView::default();
        }
        // The supervisor atomically releases transient ownership before it
        // acknowledges the mutation. Emitting a second ReleaseLease here would
        // be stale by construction and could turn a successful action into an
        // error notice.
        Vec::new()
    }

    fn mutations_blocked(&mut self, foreground_permission_pending: bool) -> bool {
        if foreground_permission_pending {
            self.notice = Some(
                "Resolve the foreground permission before mutating background runs".to_string(),
            );
            return true;
        }
        false
    }

    fn next_action_id(&mut self) -> String {
        self.next_action = self.next_action.saturating_add(1);
        format!("{}:{}", self.client_id, self.next_action)
    }

    fn release_inline_lease(&mut self, run_id: &str) -> Vec<AgentEffect> {
        let Some(lease_generation) = self
            .run_by_id(run_id)
            .filter(|run| run.lease.owned_by_client)
            .map(|run| run.lease.generation)
        else {
            return Vec::new();
        };
        if let Some(run) = self
            .snapshot
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
        {
            run.lease = AgentLeaseView::default();
        }
        vec![AgentEffect::ReleaseLease {
            run_id: run_id.to_string(),
            lease_generation,
            action_request_id: self.next_action_id(),
        }]
    }

    fn default_new_run_target(&self) -> Option<NewAgentRunTarget> {
        let agent = self.new_run_choices.agents.first()?.clone();
        let directory = self.new_run_choices.directories.first()?.clone();
        let route = self.new_run_choices.routes.first().cloned().unwrap_or(None);
        Some(NewAgentRunTarget {
            agent,
            directory: directory.directory,
            route,
            conflict: directory.conflict,
        })
    }

    fn cycle_new_run_target(&mut self, reverse: bool) {
        let Some(mut target) = self
            .new_run_target
            .clone()
            .or_else(|| self.default_new_run_target())
        else {
            self.notice = Some("No safe background target choices are available".to_string());
            return;
        };
        match self.new_run_focus {
            NewRunFocus::Agent => {
                if let Some(next) =
                    cycle_value(&self.new_run_choices.agents, &target.agent, reverse)
                {
                    target.agent = next;
                }
            }
            NewRunFocus::Directory => {
                let current = self
                    .new_run_choices
                    .directories
                    .iter()
                    .position(|choice| choice.directory == target.directory)
                    .unwrap_or_default();
                if let Some(choice) =
                    cycle_index(&self.new_run_choices.directories, current, reverse)
                {
                    target.directory = choice.directory.clone();
                    target.conflict = choice.conflict.clone();
                }
            }
            NewRunFocus::Route => {
                let current = self
                    .new_run_choices
                    .routes
                    .iter()
                    .position(|route| *route == target.route)
                    .unwrap_or_default();
                if let Some(route) = cycle_index(&self.new_run_choices.routes, current, reverse) {
                    target.route = route.clone();
                }
            }
            NewRunFocus::Prompt => {}
        }
        self.set_new_run_target(target);
    }

    fn set_new_run_target(&mut self, target: NewAgentRunTarget) {
        self.new_run_target_editors
            .agent
            .set_text(target.agent.clone());
        self.new_run_target_editors
            .directory
            .set_text(target.directory.clone());
        self.new_run_target_editors
            .route
            .set_text(target.route.clone().unwrap_or_default());
        self.new_run_target = Some(target);
    }

    fn paste_new_run_target(&mut self, text: &str) {
        let text = text.replace(['\r', '\n'], "");
        match self.new_run_focus {
            NewRunFocus::Agent => self.new_run_target_editors.agent.paste(&text),
            NewRunFocus::Directory => self.new_run_target_editors.directory.paste(&text),
            NewRunFocus::Route => self.new_run_target_editors.route.paste(&text),
            NewRunFocus::Prompt => return,
        }
        self.sync_new_run_target_from_editors();
    }

    fn edit_new_run_target(&mut self, key: KeyEvent) {
        let editor = match self.new_run_focus {
            NewRunFocus::Agent => &mut self.new_run_target_editors.agent,
            NewRunFocus::Directory => &mut self.new_run_target_editors.directory,
            NewRunFocus::Route => &mut self.new_run_target_editors.route,
            NewRunFocus::Prompt => return,
        };
        if editor.apply(key) == Edit::Changed {
            self.sync_new_run_target_from_editors();
        }
    }

    fn sync_new_run_target_from_editors(&mut self) {
        let agent = self.new_run_target_editors.agent.text().to_string();
        let directory = self.new_run_target_editors.directory.text().to_string();
        let route_text = self.new_run_target_editors.route.text().to_string();
        let conflict = self
            .new_run_choices
            .directories
            .iter()
            .find(|choice| choice.directory == directory)
            .and_then(|choice| choice.conflict.clone());
        self.new_run_target = Some(NewAgentRunTarget {
            agent,
            directory,
            route: (!route_text.trim().is_empty()).then_some(route_text),
            conflict,
        });
    }

    fn ordered_runs(&self) -> Vec<&AgentRunView> {
        let query = self.filter.to_lowercase();
        let mut runs = self
            .snapshot
            .runs
            .iter()
            .filter(|run| {
                self.foreground_run_id.as_deref() != Some(run.run_id.as_str())
                    && (self.show_stopped || group(run) != AgentGroup::Stopped)
                    && (query.is_empty()
                        || format!(
                            "{} {} {} {} {}",
                            run.label, run.agent, run.directory, run.activity, run.run_id
                        )
                        .to_lowercase()
                        .contains(&query))
            })
            .collect::<Vec<_>>();
        runs.sort_by(|left, right| {
            group(left)
                .cmp(&group(right))
                .then_with(|| right.pinned.cmp(&left.pinned))
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        runs
    }

    fn ensure_selection(&mut self) {
        let runs = self.ordered_runs();
        let selection_valid = self
            .selected_run_id
            .as_ref()
            .is_some_and(|selected| runs.iter().any(|run| run.run_id == *selected));
        if !selection_valid {
            self.selected_run_id = runs.first().map(|run| run.run_id.clone());
            self.list_scroll = 0;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let runs = self.ordered_runs();
        if runs.is_empty() {
            self.selected_run_id = None;
            return;
        }
        let current = self
            .selected_run_id
            .as_ref()
            .and_then(|selected| runs.iter().position(|run| run.run_id == *selected))
            .unwrap_or_default();
        let next = current
            .saturating_add_signed(delta)
            .min(runs.len().saturating_sub(1));
        self.selected_run_id = runs.get(next).map(|run| run.run_id.clone());
        self.list_scroll = next;
        if matches!(self.surface, AgentSurface::Permission { .. }) {
            self.surface = AgentSurface::List;
        }
    }

    fn select_edge(&mut self, end: bool) {
        let runs = self.ordered_runs();
        let index = if end { runs.len().saturating_sub(1) } else { 0 };
        self.selected_run_id = runs.get(index).map(|run| run.run_id.clone());
        self.list_scroll = index;
    }

    fn selected_run(&self) -> Option<&AgentRunView> {
        self.selected_run_id
            .as_deref()
            .and_then(|run_id| self.run_by_id(run_id))
    }

    fn run_by_id(&self, run_id: &str) -> Option<&AgentRunView> {
        self.snapshot.runs.iter().find(|run| run.run_id == run_id)
    }

    fn permission_for(&self, run_id: &str, permission_id: &str) -> Option<&AgentPermissionView> {
        self.run_by_id(run_id).and_then(|run| {
            if let AgentAttention::Permission(permission) = &run.attention
                && permission.permission_id == permission_id
            {
                Some(permission)
            } else {
                None
            }
        })
    }

    fn focused_permission_identity(&self) -> Option<(String, String, u64)> {
        let AgentSurface::Permission {
            run_id,
            permission_id,
            lease_generation,
            ..
        } = &self.surface
        else {
            return None;
        };
        let run = self.run_by_id(run_id)?;
        let AgentAttention::Permission(permission) = &run.attention else {
            return None;
        };
        (permission.permission_id == *permission_id).then(|| {
            (
                run_id.clone(),
                permission_id.clone(),
                run.lease.generation.max(*lease_generation),
            )
        })
    }

    fn collapsed_summary(&self) -> CollapsedSummary {
        let mut summary = CollapsedSummary::default();
        for run in &self.snapshot.runs {
            if self.foreground_run_id.as_deref() == Some(run.run_id.as_str()) {
                continue;
            }
            match group(run) {
                AgentGroup::NeedsInput => {
                    summary.needs_input = summary.needs_input.saturating_add(1);
                    if summary.urgent.is_none() {
                        summary.urgent = Some((run.label.clone(), attention_label(run)));
                    }
                }
                AgentGroup::Ready => summary.ready = summary.ready.saturating_add(1),
                AgentGroup::Working => summary.working = summary.working.saturating_add(1),
                AgentGroup::Idle | AgentGroup::Stopped => {}
            }
        }
        summary.total = self
            .snapshot
            .runs
            .iter()
            .filter(|run| {
                self.foreground_run_id.as_deref() != Some(run.run_id.as_str())
                    && group(run) != AgentGroup::Stopped
            })
            .count();
        summary
    }

    /// Draw the one-line event-derived ambient strip.
    pub fn render_collapsed(&self, frame: &mut Frame<'_>, area: Rect) {
        let summary = self.collapsed_summary();
        let width = usize::from(area.width);
        let text = if width < 68 {
            format!(
                "BG !{} · ●{} · ◌{} · /",
                summary.needs_input, summary.ready, summary.working
            )
        } else if summary.total == 0 {
            "BG no background agents · / Agents".to_string()
        } else if let Some((label, attention)) = summary.urgent {
            let additional = summary.needs_input.saturating_sub(1);
            format!(
                "BG ! {label} {attention} · +{additional} attention · {} ready · {} working · / Agents",
                summary.ready, summary.working
            )
        } else {
            format!(
                "BG {} ready · {} working · / Agents",
                summary.ready, summary.working
            )
        };
        frame.render_widget(
            Paragraph::new(truncate_cells(&text, width)).style(if summary.needs_input > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            }),
            area,
        );
    }

    /// Draw the bounded normal-buffer command center. `foreground_lines` is
    /// only a count; the foreground draft bytes never enter this reducer.
    pub fn render_expanded(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        foreground_lines: usize,
        foreground_permission_pending: bool,
    ) -> Option<Position> {
        frame.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Foreground draft preserved · {} {} ",
            foreground_lines,
            if foreground_lines == 1 {
                "line"
            } else {
                "lines"
            }
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return None;
        }
        if let AgentSurface::Permission {
            run_id,
            permission_id,
            selected,
            ..
        } = &self.surface
        {
            self.render_permission(frame, inner, run_id, permission_id, *selected);
            return None;
        }
        let mut reserved = 2_u16;
        if foreground_permission_pending || self.notice.is_some() {
            reserved = reserved.saturating_add(1);
        }
        let body_height = inner.height.saturating_sub(reserved).max(1);
        let [heading, body, notice, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(body_height),
            Constraint::Length(u16::from(
                foreground_permission_pending || self.notice.is_some(),
            )),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.render_heading(frame, heading);
        let cursor = match &self.surface {
            AgentSurface::List => {
                self.render_rows(frame, body);
                None
            }
            AgentSurface::Peek => {
                self.render_peek(frame, body, foreground_permission_pending);
                None
            }
            AgentSurface::Reply { run_id } => self.render_reply(frame, body, run_id),
            AgentSurface::NewRun => self.render_new_run(frame, body),
            AgentSurface::Permission {
                run_id,
                permission_id,
                selected,
                ..
            } => {
                self.render_permission(frame, body, run_id, permission_id, *selected);
                None
            }
            AgentSurface::Confirm { run_id, intent } => {
                self.render_confirmation(frame, body, run_id, *intent);
                None
            }
            AgentSurface::TakeoverConfirm { run_id, selected } => {
                self.render_takeover_confirmation(frame, body, run_id, *selected);
                None
            }
            AgentSurface::ExitConfirm { selected } => {
                self.render_exit_confirmation(frame, body, *selected);
                None
            }
            AgentSurface::Collapsed | AgentSurface::Inspector(_) => None,
        };
        if foreground_permission_pending {
            frame.render_widget(
                Paragraph::new("Foreground permission waiting · background actions disabled")
                    .style(Style::default().fg(Color::Yellow)),
                notice,
            );
        } else if let Some(message) = &self.notice {
            frame.render_widget(
                Paragraph::new(truncate_cells(message, usize::from(notice.width)))
                    .style(Style::default().fg(Color::Yellow)),
                notice,
            );
        }
        frame.render_widget(
            Paragraph::new(self.help()).style(Style::default().fg(Color::DarkGray)),
            help,
        );
        cursor
    }

    fn render_heading(&self, frame: &mut Frame<'_>, area: Rect) {
        let summary = self.collapsed_summary();
        let filter = if self.filtering {
            format!(" · filter: {}_", self.filter)
        } else if self.filter.is_empty() {
            String::new()
        } else {
            format!(" · filter: {}", self.filter)
        };
        frame.render_widget(
            Paragraph::new(truncate_cells(
                &format!(
                    "Agents · {} need input · {} ready · {} working{filter}",
                    summary.needs_input, summary.ready, summary.working
                ),
                usize::from(area.width),
            ))
            .style(Style::default().add_modifier(Modifier::BOLD)),
            area,
        );
    }

    fn render_rows(&self, frame: &mut Frame<'_>, area: Rect) {
        let runs = self.ordered_runs();
        if runs.is_empty() {
            let message = if self.snapshot.runs.is_empty() {
                "No background agents"
            } else {
                "No agents match this filter"
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        let visible = usize::from(area.height).max(1);
        let selected = self
            .selected_run_id
            .as_ref()
            .and_then(|id| runs.iter().position(|run| run.run_id == *id))
            .unwrap_or_default();
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .max(self.list_scroll.saturating_sub(visible.saturating_sub(1)))
            .min(runs.len().saturating_sub(visible));
        let lines = runs
            .iter()
            .skip(start)
            .take(visible)
            .map(|run| {
                let selected = self.selected_run_id.as_deref() == Some(run.run_id.as_str());
                let marker = if selected { "›" } else { " " };
                let state = group_symbol(group(run));
                let age = run
                    .age_label
                    .as_deref()
                    .map(|age| format!(" · {age}"))
                    .unwrap_or_default();
                let line = format!(
                    "{marker} {state} {} · {} · {}{age}",
                    run.label,
                    run.agent,
                    activity_label(run)
                );
                Line::styled(
                    truncate_cells(&line, usize::from(area.width)),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_peek(&self, frame: &mut Frame<'_>, area: Rect, foreground_permission_pending: bool) {
        let Some(run) = self.selected_run() else {
            return;
        };
        let lease = run
            .lease
            .owner
            .as_deref()
            .filter(|_| !run.lease.owned_by_client)
            .map(|owner| format!(" · read only: {owner}"))
            .unwrap_or_default();
        let mut lines = vec![Line::styled(
            truncate_cells(
                &format!(
                    "{} · {} · {}{lease}",
                    run.label,
                    group_label(group(run)),
                    run.directory
                ),
                usize::from(area.width),
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if area.height >= 3 {
            let metadata = run_metadata(run).join(" · ");
            if !metadata.is_empty() {
                lines.extend(
                    wrap(&Line::from(metadata), area.width.max(1))
                        .into_iter()
                        .take(usize::from(area.height).saturating_sub(2))
                        .map(|line| line.style(Style::default().fg(Color::DarkGray))),
                );
            }
        }
        lines.extend(
            wrap(&Line::from(attention_detail(run)), area.width.max(1))
                .into_iter()
                .take(usize::from(area.height).saturating_sub(lines.len())),
        );
        if foreground_permission_pending {
            lines.push(Line::styled(
                "Read-only while foreground permission waits",
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_reply(&self, frame: &mut Frame<'_>, area: Rect, run_id: &str) -> Option<Position> {
        let run = self.run_by_id(run_id)?;
        let editor = self.reply_drafts.get(run_id);
        let text = editor.map(Editor::text).unwrap_or_default();
        let [title, input] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(format!("Reply to {} · background", run.label))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            title,
        );
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::LEFT))
                .wrap(Wrap { trim: false }),
            input,
        );
        editor_cursor(input, editor)
    }

    fn render_new_run(&self, frame: &mut Frame<'_>, area: Rect) -> Option<Position> {
        let target = self.new_run_target.as_ref();
        let summary = match self.new_run_focus {
            NewRunFocus::Agent => {
                "New background · editing agent · ↑↓ choices · Tab next".to_string()
            }
            NewRunFocus::Directory => target
                .and_then(|target| target.conflict.as_deref())
                .map_or_else(
                    || "New background · editing directory · ↑↓ choices · Tab next".to_string(),
                    |conflict| format!("New background · editing directory · BLOCKED {conflict}"),
                ),
            NewRunFocus::Route => {
                "New background · editing route · empty uses routing policy · Tab next".to_string()
            }
            NewRunFocus::Prompt => target.map_or_else(
                || "New background prompt · target required · Tab selects target".to_string(),
                |target| {
                    let route = target.route.as_deref().unwrap_or("routing policy");
                    let conflict = target
                        .conflict
                        .as_deref()
                        .map(|value| format!(" · BLOCKED {value}"))
                        .unwrap_or_default();
                    format!(
                        "New background prompt · {} · {} · {route}{conflict}",
                        target.agent, target.directory
                    )
                },
            ),
        };
        let [title, input] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(truncate_cells(&summary, usize::from(title.width)))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            title,
        );
        let editor = match self.new_run_focus {
            NewRunFocus::Agent => &self.new_run_target_editors.agent,
            NewRunFocus::Directory => &self.new_run_target_editors.directory,
            NewRunFocus::Route => &self.new_run_target_editors.route,
            NewRunFocus::Prompt => &self.new_run_draft,
        };
        let horizontal = if self.new_run_focus == NewRunFocus::Prompt {
            0
        } else {
            single_line_editor_scroll(editor, input.width.saturating_sub(1))
        };
        if self.new_run_focus == NewRunFocus::Prompt {
            frame.render_widget(
                Paragraph::new(editor.text())
                    .block(Block::default().borders(Borders::LEFT))
                    .wrap(Wrap { trim: false }),
                input,
            );
            editor_cursor(input, Some(editor))
        } else {
            frame.render_widget(
                Paragraph::new(editor.text())
                    .block(Block::default().borders(Borders::LEFT))
                    .scroll((0, horizontal)),
                input,
            );
            single_line_editor_cursor(input, editor, horizontal)
        }
    }

    fn render_permission(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        run_id: &str,
        permission_id: &str,
        selected: Option<usize>,
    ) {
        let Some(permission) = self.permission_for(run_id, permission_id) else {
            return;
        };
        let label = self
            .run_by_id(run_id)
            .map(|run| run.label.as_str())
            .unwrap_or(run_id);
        let mut lines = wrap(
            &Line::styled(
                format!("{label} · Permission · {}", permission.title),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            area.width.max(1),
        );
        for (index, option) in permission.options.iter().enumerate() {
            let marker = if selected == Some(index) { "›" } else { " " };
            let style = if selected == Some(index) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::styled(
                truncate_cells(
                    &format!("{marker} [{}] {}", index.saturating_add(1), option.label),
                    usize::from(area.width),
                ),
                style,
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_confirmation(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        run_id: &str,
        intent: AgentLeaseIntent,
    ) {
        let label = self
            .run_by_id(run_id)
            .map(|run| run.label.as_str())
            .unwrap_or(run_id);
        let action = match intent {
            AgentLeaseIntent::Cancel => "cancel the active turn",
            AgentLeaseIntent::MarkReviewed => "mark the result reviewed",
            AgentLeaseIntent::Stop => "stop the supervised run",
            AgentLeaseIntent::Reply => "send the reply",
            AgentLeaseIntent::Permission => "answer the permission",
            AgentLeaseIntent::Attach => "attach",
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Confirm {action} for {label}?\nEnter confirms · Esc returns"
            ))
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_takeover_confirmation(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        run_id: &str,
        selected: bool,
    ) {
        let Some(run) = self.run_by_id(run_id) else {
            return;
        };
        let owner = run.lease.owner.as_deref().unwrap_or("another client");
        let marker = if selected { "› [x]" } else { "  [ ]" };
        frame.render_widget(
            Paragraph::new(format!(
                "{} is controlled by {owner}. Taking over invalidates that client's pending actions.\n\n{marker} Take over control\n\nSelect explicitly, then press Enter · Esc returns",
                run.label
            ))
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_exit_confirmation(&self, frame: &mut Frame<'_>, area: Rect, selected: Option<usize>) {
        let keep = if selected == Some(0) { "›" } else { " " };
        let discard = if selected == Some(1) { "›" } else { " " };
        frame.render_widget(
            Paragraph::new(format!(
                "Unsent background drafts are still bound to their runs.\n\n{keep} [1] Keep editing\n{discard} [2] Discard drafts and exit\n\nSelect explicitly, then press Enter · Esc returns"
            ))
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn help(&self) -> &'static str {
        if self.mode == AgentDeckMode::Inline {
            return match self.surface {
                AgentSurface::List => "↑↓ Select · Enter Attach · / Commands · Esc Collapse",
                AgentSurface::Peek => "/ Commands · Enter Attach · Esc Back",
                AgentSurface::Reply { .. } => {
                    "Enter Send to named background · / Commands · Esc keep draft"
                }
                AgentSurface::NewRun => {
                    "Tab Target field · Enter Dispatch · / Commands · Esc keep draft"
                }
                AgentSurface::Permission { .. } => {
                    "↑↓ or number selects · Enter confirms · Esc close"
                }
                AgentSurface::Confirm { .. } => "Enter Confirm · Esc return",
                AgentSurface::TakeoverConfirm { .. } => "↑↓ select · Enter confirms · Esc return",
                AgentSurface::ExitConfirm { .. } => "↑↓ select · Enter applies · Esc return",
                AgentSurface::Collapsed | AgentSurface::Inspector(_) => "",
            };
        }
        match self.surface {
            AgentSurface::List => {
                "↑↓ Select · / Filter · Space Peek · R Reply · T Takeover · Enter Attach · N New · F5 Collapse"
            }
            AgentSurface::Peek => {
                "F2 Permission · R Reply · C Cancel · M Reviewed · S Stop · T Takeover · Enter Attach"
            }
            AgentSurface::Reply { .. } => {
                "Enter Send to named background · Esc keep draft and return"
            }
            AgentSurface::NewRun => {
                "Tab Target field · ↑↓ Choose · Enter Dispatch prompt · Esc keep draft"
            }
            AgentSurface::Permission { .. } => {
                "↑↓ or number selects · Enter confirms exact option · Esc close"
            }
            AgentSurface::Confirm { .. } => "Enter Confirm · Esc return",
            AgentSurface::TakeoverConfirm { .. } => {
                "↑↓ or Space select · Enter confirmed takeover · Esc return"
            }
            AgentSurface::ExitConfirm { .. } => "↑↓ or number selects · Enter applies · Esc return",
            AgentSurface::Collapsed | AgentSurface::Inspector(_) => "",
        }
    }

    /// Draw retained background history/detail in an alternate screen.
    pub fn render_inspector(&self, frame: &mut Frame<'_>, foreground_permission_pending: bool) {
        let AgentSurface::Inspector(inspector) = &self.surface else {
            return;
        };
        let area = frame.area();
        frame.render_widget(Clear, area);
        let [banner, body, help] = Layout::vertical([
            Constraint::Length(u16::from(foreground_permission_pending)),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        if foreground_permission_pending {
            frame.render_widget(
                Paragraph::new("Foreground permission waiting · / to return")
                    .style(Style::default().fg(Color::Yellow)),
                banner,
            );
        }
        let title = if inspector.searching {
            format!(
                " Agent Inspector / {} · search: {}_ ",
                inspector.run.label, inspector.search
            )
        } else if inspector.search.is_empty() {
            format!(
                " Agent Inspector / {} · {} · background ",
                inspector.run.label, inspector.run.agent
            )
        } else {
            format!(
                " Agent Inspector / {} · {} matches ",
                inspector.run.label,
                history_match_count(&inspector.content(), &inspector.search)
            )
        };
        let content = inspector.content();
        let visible_history_rows = usize::from(body.height.saturating_sub(2)).max(1);
        let scroll_top = inspector
            .scroll
            .saturating_sub(visible_history_rows.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(
                content
                    .lines()
                    .map(|line| Line::from(sanitize(line)))
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((u16::try_from(scroll_top).unwrap_or(u16::MAX), 0))
            .wrap(Wrap { trim: false }),
            body,
        );
        if inspector.permission_focused
            && let AgentAttention::Permission(permission) = &inspector.run.attention
        {
            let panel = body;
            frame.render_widget(Clear, panel);
            let block = Block::default().borders(Borders::ALL).title(" Permission ");
            let inner = block.inner(panel);
            frame.render_widget(block, panel);
            let visible = usize::from(inner.height);
            let mut lines = wrap(
                &Line::styled(
                    sanitize(&permission.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                inner.width.max(1),
            );
            if !permission.detail.trim().is_empty() {
                lines.push(Line::default());
                for detail in permission.detail.lines() {
                    lines.extend(wrap(&Line::from(sanitize(detail)), inner.width.max(1)));
                }
            }
            lines.push(Line::default());
            let mut selected_range = None;
            for (index, option) in permission.options.iter().enumerate() {
                let focused = inspector.permission_selected == Some(index);
                let start = lines.len();
                lines.extend(wrap(
                    &Line::styled(
                        format!(
                            "{} [{}] {}",
                            if focused { "›" } else { " " },
                            index.saturating_add(1),
                            sanitize(&option.label)
                        ),
                        if focused {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    inner.width.max(1),
                ));
                if focused {
                    selected_range = Some((start, lines.len()));
                }
            }
            let maximum = lines.len().saturating_sub(visible);
            let start = if inspector.permission_scroll > 0 {
                inspector.permission_scroll.min(maximum)
            } else if let Some((selected_start, selected_end)) = selected_range {
                let centered = selected_start.saturating_sub(visible.saturating_div(2));
                let mut start = centered.min(maximum);
                if selected_end.saturating_sub(start) > visible {
                    start = selected_start.min(maximum);
                }
                start
            } else {
                0
            };
            frame.render_widget(
                Paragraph::new(lines)
                    .scroll((u16::try_from(start).unwrap_or(u16::MAX), 0))
                    .wrap(Wrap { trim: false }),
                inner,
            );
        }
        frame.render_widget(
            Paragraph::new(if inspector.permission_focused {
                "↑↓ select · PgUp/PgDn scroll · Enter confirms exact option · Esc closes"
            } else if self.mode == AgentDeckMode::Inline {
                "/ Commands (permission, search, copy, export, detach) · ↑↓ Scroll"
            } else {
                "F2 Permission · ↑↓ Scroll · Ctrl-F Search · Ctrl-Y Copy · Ctrl-E Export · F5 Detach"
            })
            .style(Style::default().fg(Color::DarkGray)),
            help,
        );
    }
}

impl Default for AgentDeckState {
    fn default() -> Self {
        Self::new(AgentDeckMode::Inline, "code-client")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CollapsedSummary {
    urgent: Option<(String, String)>,
    needs_input: usize,
    ready: usize,
    working: usize,
    total: usize,
}

fn group(run: &AgentRunView) -> AgentGroup {
    if matches!(
        run.attention,
        AgentAttention::Question { .. }
            | AgentAttention::Permission(_)
            | AgentAttention::Error { .. }
    ) {
        return AgentGroup::NeedsInput;
    }
    if matches!(run.attention, AgentAttention::Result { .. })
        && run.review == AgentReviewState::Unread
    {
        return AgentGroup::Ready;
    }
    if matches!(
        run.turn,
        AgentTurnState::Submitting | AgentTurnState::Working | AgentTurnState::Cancelling
    ) || matches!(
        run.process,
        AgentProcessState::Starting | AgentProcessState::Stopping
    ) {
        return AgentGroup::Working;
    }
    if matches!(
        run.process,
        AgentProcessState::Stopped | AgentProcessState::Interrupted
    ) {
        AgentGroup::Stopped
    } else if run.process == AgentProcessState::Failed {
        AgentGroup::NeedsInput
    } else {
        AgentGroup::Idle
    }
}

fn group_symbol(group: AgentGroup) -> &'static str {
    match group {
        AgentGroup::NeedsInput => "!",
        AgentGroup::Ready => "●",
        AgentGroup::Working => "◌",
        AgentGroup::Idle => "·",
        AgentGroup::Stopped => "×",
    }
}

fn group_label(group: AgentGroup) -> &'static str {
    match group {
        AgentGroup::NeedsInput => "Needs input",
        AgentGroup::Ready => "Ready for review",
        AgentGroup::Working => "Working",
        AgentGroup::Idle => "Idle",
        AgentGroup::Stopped => "Stopped",
    }
}

fn attention_label(run: &AgentRunView) -> String {
    match &run.attention {
        AgentAttention::None => group_label(group(run)).to_lowercase(),
        AgentAttention::Question { .. } => "needs input".to_string(),
        AgentAttention::Permission(_) => "permission".to_string(),
        AgentAttention::Result { .. } => "ready".to_string(),
        AgentAttention::Error { .. } => "failed".to_string(),
    }
}

fn activity_label(run: &AgentRunView) -> String {
    match &run.attention {
        AgentAttention::Question { title, .. } => format!("Question: {title}"),
        AgentAttention::Permission(permission) => format!("Permission: {}", permission.title),
        AgentAttention::Result { .. } => "Turn ready for review".to_string(),
        AgentAttention::Error { message } => message.clone(),
        AgentAttention::None => run.activity.clone(),
    }
}

fn attention_detail(run: &AgentRunView) -> String {
    match &run.attention {
        AgentAttention::None => run.activity.clone(),
        AgentAttention::Question { detail, .. } => detail.clone(),
        AgentAttention::Permission(permission) => {
            if permission.detail.trim().is_empty() {
                permission.title.clone()
            } else {
                format!("{}\n{}", permission.title, permission.detail)
            }
        }
        AgentAttention::Result { summary } => summary.clone(),
        AgentAttention::Error { message } => message.clone(),
    }
}

fn permission_needs_inspector(permission: &AgentPermissionView) -> bool {
    permission.requires_inspector
        || !permission.detail.trim().is_empty()
        || permission.options.len() > 3
        || permission.title.width() > 24
        || permission
            .options
            .iter()
            .any(|option| option.label.width() > 32)
}

fn previous_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.unwrap_or_default().saturating_sub(1))
}

fn cycle_index<T>(values: &[T], current: usize, reverse: bool) -> Option<&T> {
    if values.is_empty() {
        return None;
    }
    let index = if reverse {
        current
            .checked_sub(1)
            .unwrap_or(values.len().saturating_sub(1))
    } else {
        current.saturating_add(1) % values.len()
    };
    values.get(index)
}

fn cycle_value(values: &[String], current: &str, reverse: bool) -> Option<String> {
    let index = values
        .iter()
        .position(|value| value == current)
        .unwrap_or_default();
    cycle_index(values, index, reverse).cloned()
}

fn next_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(
        selected
            .map(|value| value.saturating_add(1))
            .unwrap_or_default()
            .min(len.saturating_sub(1)),
    )
}

fn run_metadata(run: &AgentRunView) -> Vec<String> {
    vec![
        format!("Agent: {}", sanitize(&run.agent)),
        format!(
            "Native session: {}",
            run.native_session_id
                .as_deref()
                .map(sanitize)
                .unwrap_or_else(|| "not reported".to_string())
        ),
        format!(
            "Confirmed route: {}",
            run.confirmed_route
                .as_deref()
                .map(sanitize)
                .unwrap_or_else(|| "unknown (not reported)".to_string())
        ),
        format!(
            "Attributed cost: {}",
            run.attributed_cost
                .as_deref()
                .map(sanitize)
                .unwrap_or_else(|| "unknown (no attribution reported)".to_string())
        ),
    ]
}

fn visible_history_blocks(events: &[AgentHistoryEvent]) -> Vec<VisibleHistoryBlock> {
    let mut blocks: Vec<VisibleHistoryBlock> = Vec::new();
    for event in events {
        let streamed = matches!(
            event.kind,
            AgentHistoryKind::Assistant | AgentHistoryKind::Thought
        );
        if streamed
            && let Some(previous) = blocks.last_mut()
            && previous.kind == event.kind
        {
            previous.last_seq = event.seq;
            previous.text.push_str(&event.text);
            continue;
        }
        blocks.push(VisibleHistoryBlock {
            kind: event.kind,
            first_seq: event.seq,
            last_seq: event.seq,
            text: event.text.clone(),
        });
    }
    blocks
}

fn visible_inspector_content(
    run: &AgentRunView,
    events: &[AgentHistoryEvent],
    history_complete: bool,
) -> String {
    let mut sections = run_metadata(run);
    let history = visible_history_content(events, history_complete);
    if !history.is_empty() {
        sections.push(String::new());
        sections.push(history);
    }
    sections.join("\n")
}

fn visible_history_content(events: &[AgentHistoryEvent], history_complete: bool) -> String {
    let mut lines = Vec::new();
    if !history_complete {
        lines.push("Earlier activity is not retained by BitRouter".to_string());
        lines.push(String::new());
    }
    for block in visible_history_blocks(events) {
        let label = match block.kind {
            AgentHistoryKind::User => "You",
            AgentHistoryKind::Assistant => "Assistant",
            AgentHistoryKind::Thought => "Thinking",
            AgentHistoryKind::Tool => "Tool",
            AgentHistoryKind::Status => "Status",
            AgentHistoryKind::Permission => "Permission",
            AgentHistoryKind::Result => "Ready for review",
            AgentHistoryKind::Error => "Error",
        };
        let sequence = if block.first_seq == block.last_seq {
            format!("#{}", block.first_seq)
        } else {
            format!("#{}–{}", block.first_seq, block.last_seq)
        };
        lines.push(format!("{label} · {sequence}"));
        lines.extend(block.text.lines().map(sanitize));
        lines.push(String::new());
    }
    lines.join("\n")
}

fn first_history_match(content: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let query = query.to_lowercase();
    content
        .lines()
        .position(|line| line.to_lowercase().contains(&query))
}

fn history_match_count(content: &str, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    let query = query.to_lowercase();
    content.to_lowercase().matches(&query).count()
}

fn single_line_editor_scroll(editor: &Editor, width: u16) -> u16 {
    let before = editor
        .text()
        .get(..editor.cursor_byte())
        .unwrap_or_default();
    let cursor = u16::try_from(before.width()).unwrap_or(u16::MAX);
    cursor.saturating_sub(width.saturating_sub(1))
}

fn single_line_editor_cursor(area: Rect, editor: &Editor, horizontal: u16) -> Option<Position> {
    if area.width < 2 || area.height == 0 {
        return None;
    }
    let before = editor
        .text()
        .get(..editor.cursor_byte())
        .unwrap_or_default();
    let cursor = u16::try_from(before.width()).unwrap_or(u16::MAX);
    Some(Position::new(
        area.x.saturating_add(1).saturating_add(
            cursor
                .saturating_sub(horizontal)
                .min(area.width.saturating_sub(2)),
        ),
        area.y,
    ))
}

fn editor_cursor(area: Rect, editor: Option<&Editor>) -> Option<Position> {
    let editor = editor?;
    if area.width < 2 || area.height == 0 {
        return None;
    }
    let before = editor
        .text()
        .get(..editor.cursor_byte())
        .unwrap_or_default();
    let tail = before.rsplit('\n').next().unwrap_or_default();
    let x_offset = u16::try_from(tail.width())
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(2));
    let y_offset = u16::try_from(before.lines().count().saturating_sub(1))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(1));
    Some(Position::new(
        area.x.saturating_add(1).saturating_add(x_offset),
        area.y.saturating_add(y_offset),
    ))
}

fn pressed(event: &Event) -> Option<&KeyEvent> {
    let Event::Key(key) = event else {
        return None;
    };
    (key.kind == KeyEventKind::Press).then_some(key)
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '�'
            }
        })
        .collect()
}

fn truncate_cells(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let ellipsis = "…";
    let target = width.saturating_sub(ellipsis.width());
    let mut rendered = String::new();
    for character in text.chars() {
        let next = character.width().unwrap_or_default();
        if rendered.width().saturating_add(next) > target {
            break;
        }
        rendered.push(character);
    }
    rendered.push_str(ellipsis);
    rendered
}

/// Alternate-screen custody for standalone `bro agents`. Inline Code uses its
/// existing [`crate::code::CodeView`] and the same reducer state.
pub struct AgentDeckView {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    finished: bool,
}

impl AgentDeckView {
    pub fn open() -> io::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bro agents requires an interactive stdin and stdout; use sessions --json",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        let mut stdout = std::io::stdout();
        if let Err(error) = crate::lifecycle::enable_session_keys()
            .and_then(|()| crate::lifecycle::enter_alternate_screen())
            .and_then(|()| execute!(stdout, Hide))
        {
            crate::lifecycle::restore();
            return Err(error);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                crate::lifecycle::restore();
                return Err(error);
            }
        };
        Ok(Self {
            terminal,
            finished: false,
        })
    }

    pub fn draw(
        &mut self,
        state: &AgentDeckState,
        foreground_permission_pending: bool,
    ) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.terminal
            .draw(|frame| {
                if state.is_inspector() {
                    state.render_inspector(frame, foreground_permission_pending);
                } else {
                    let area = frame.area();
                    let cursor =
                        state.render_expanded(frame, area, 0, foreground_permission_pending);
                    if let Some(position) = cursor {
                        frame.set_cursor_position(position);
                    }
                }
            })
            .map(|_| ())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if !self.finished {
            crate::lifecycle::restore();
            self.finished = true;
        }
        Ok(())
    }
}

impl Drop for AgentDeckView {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> AgentAction {
        AgentAction::TestLegacyKey(code)
    }

    fn run(id: &str, label: &str) -> AgentRunView {
        AgentRunView::new(id, label, "codex", "/repo")
    }

    fn state_with(runs: Vec<AgentRunView>) -> AgentDeckState {
        let mut state = AgentDeckState::new(AgentDeckMode::Inline, "client-a");
        let _ = state.replace_snapshot(AgentDeckSnapshot {
            sequence: 1,
            runs,
            new_run_target: Some(NewAgentRunTarget {
                agent: "codex".to_string(),
                directory: "/repo-worktree".to_string(),
                route: Some("bitrouter/auto".to_string()),
                conflict: None,
            }),
        });
        state
    }

    #[test]
    fn inline_deck_has_no_default_letter_action_but_named_reply_works() {
        let mut state = state_with(vec![run("r1", "review")]);
        let _ = state.command(AgentDeckCommand::Open, false);
        let raw = AgentAction::Event(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        )));
        assert!(state.step(raw, false).is_empty());
        assert!(matches!(state.surface, AgentSurface::List));
        assert!(matches!(
            state.command(AgentDeckCommand::Reply, false).as_slice(),
            [AgentEffect::AcquireLease {
                intent: AgentLeaseIntent::Reply,
                ..
            }]
        ));
    }

    fn grid(backend: &TestBackend) -> String {
        let buffer = backend.buffer();
        (buffer.area.top()..buffer.area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut x = buffer.area.left();
                while x < buffer.area.right() {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x = x.saturating_add(u16::try_from(symbol.width()).unwrap_or(1).max(1));
                }
                row
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collapsed_summary_changes_only_for_meaningful_transitions() {
        let mut working = run("r1", "tests");
        working.turn = AgentTurnState::Working;
        working.activity = "Reading one".to_string();
        let mut state = state_with(vec![working.clone()]);

        working.activity = "Reading two".to_string();
        working.last_seq = 2;
        assert!(!state.replace_snapshot(AgentDeckSnapshot {
            sequence: 2,
            runs: vec![working.clone()],
            new_run_target: None,
        }));

        working.attention = AgentAttention::Result {
            summary: "Review this".to_string(),
        };
        working.review = AgentReviewState::Unread;
        working.turn = AgentTurnState::Idle;
        assert!(state.replace_snapshot(AgentDeckSnapshot {
            sequence: 3,
            runs: vec![working],
            new_run_target: None,
        }));
    }

    #[test]
    fn selection_stays_on_identity_when_rows_change_groups() {
        let mut first = run("r1", "first");
        let second = run("r2", "second");
        let mut state = state_with(vec![first.clone(), second.clone()]);
        state.toggle();
        let _ = state.step(press(KeyCode::Down), false);
        assert_eq!(state.selected_run_id(), Some("r2"));

        first.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "p1".to_string(),
            title: "Run tests".to_string(),
            detail: String::new(),
            options: Vec::new(),
            requires_inspector: false,
        });
        let _ = state.replace_snapshot(AgentDeckSnapshot {
            sequence: 2,
            runs: vec![first, second],
            new_run_target: None,
        });
        assert_eq!(state.selected_run_id(), Some("r2"));
    }

    #[test]
    fn background_reply_is_target_bound_and_foreground_safe() {
        let mut state = state_with(vec![run("r1", "docs"), run("r2", "tests")]);
        state.toggle();
        let acquire = state.step(press(KeyCode::Char('r')), false);
        let action_request_id = match acquire.as_slice() {
            [
                AgentEffect::AcquireLease {
                    run_id,
                    action_request_id,
                    intent: AgentLeaseIntent::Reply,
                },
            ] if run_id == "r1" => action_request_id.clone(),
            _ => String::new(),
        };
        assert!(!action_request_id.is_empty());
        let _ = state.step(
            AgentAction::LeaseAcquired {
                run_id: "r1".to_string(),
                intent: AgentLeaseIntent::Reply,
                generation: 7,
                action_request_id,
            },
            false,
        );
        let _ = state.step(press(KeyCode::Char('是')), false);
        let send = state.step(press(KeyCode::Enter), false);
        let accepted_id = match send.as_slice() {
            [
                AgentEffect::Reply {
                    run_id,
                    prompt,
                    lease_generation: 7,
                    action_request_id,
                },
            ] if run_id == "r1" && prompt == "是" => action_request_id.clone(),
            _ => String::new(),
        };
        assert!(matches!(
            send.as_slice(),
            [AgentEffect::Reply {
                run_id,
                prompt,
                lease_generation: 7,
                ..
            }] if run_id == "r1" && prompt == "是"
        ));
        assert!(!accepted_id.is_empty());
        assert!(
            state
                .step(
                    AgentAction::MutationAccepted {
                        action_request_id: accepted_id,
                    },
                    false,
                )
                .is_empty()
        );
        assert!(
            !state
                .run_by_id("r1")
                .is_some_and(|run| run.lease.owned_by_client)
        );
        assert!(!state.has_unsent_drafts());
    }

    #[test]
    fn permission_focus_starts_without_a_selection_and_is_generation_fenced() {
        let mut permission_run = run("r1", "auth");
        permission_run.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run cargo nextest".to_string(),
            detail: String::new(),
            options: vec![
                AgentPermissionOption {
                    id: "once".to_string(),
                    label: "Allow once".to_string(),
                },
                AgentPermissionOption {
                    id: "deny".to_string(),
                    label: "Deny".to_string(),
                },
            ],
            requires_inspector: false,
        });
        permission_run.lease = AgentLeaseView {
            generation: 11,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![permission_run]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char(' ')), false);
        let _ = state.step(press(KeyCode::F(2)), false);
        assert!(state.step(press(KeyCode::Enter), false).is_empty());
        let _ = state.step(press(KeyCode::Down), false);
        let effect = state.step(press(KeyCode::Enter), false);
        assert!(matches!(
            effect.as_slice(),
            [AgentEffect::RespondPermission {
                run_id,
                permission_id,
                option_id,
                lease_generation: 11,
                ..
            }] if run_id == "r1" && permission_id == "perm-1" && option_id == "once"
        ));
    }

    #[test]
    fn foreground_permission_blocks_background_mutations() {
        let mut state = state_with(vec![run("r1", "docs")]);
        state.toggle();
        assert!(state.step(press(KeyCode::Char('r')), true).is_empty());
        assert!(matches!(state.surface, AgentSurface::List));
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("foreground"))
        );
    }

    #[test]
    fn foreground_permission_during_lease_acquisition_releases_without_acting() {
        let mut state = state_with(vec![run("r1", "docs")]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char('c')), false);
        let acquire = state.step(press(KeyCode::Enter), false);
        let action_request_id = match acquire.as_slice() {
            [
                AgentEffect::AcquireLease {
                    run_id,
                    intent: AgentLeaseIntent::Cancel,
                    action_request_id,
                },
            ] if run_id == "r1" => action_request_id.clone(),
            _ => String::new(),
        };
        assert!(!action_request_id.is_empty());
        let effects = state.step(
            AgentAction::LeaseAcquired {
                run_id: "r1".to_string(),
                intent: AgentLeaseIntent::Cancel,
                generation: 12,
                action_request_id,
            },
            true,
        );
        assert!(matches!(
            effects.as_slice(),
            [AgentEffect::ReleaseLease {
                run_id,
                lease_generation: 12,
                ..
            }] if run_id == "r1"
        ));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            AgentEffect::CancelTurn { .. }
                | AgentEffect::MarkReviewed { .. }
                | AgentEffect::Stop { .. }
        )));
    }

    #[test]
    fn cancelled_reply_acquisition_releases_a_late_lease_without_redirecting_draft() {
        let mut state = state_with(vec![run("r1", "docs")]);
        state.toggle();
        let acquire = state.step(press(KeyCode::Char('r')), false);
        let action_request_id = match acquire.as_slice() {
            [
                AgentEffect::AcquireLease {
                    run_id,
                    intent: AgentLeaseIntent::Reply,
                    action_request_id,
                },
            ] if run_id == "r1" => action_request_id.clone(),
            _ => String::new(),
        };
        assert!(!action_request_id.is_empty());
        assert!(
            state
                .step(
                    AgentAction::Event(Event::Paste("keep this draft".to_string())),
                    false,
                )
                .is_empty()
        );
        assert!(state.step(press(KeyCode::Enter), false).is_empty());
        assert!(state.step(press(KeyCode::Esc), false).is_empty());
        assert!(matches!(state.surface, AgentSurface::Peek));

        let effects = state.step(
            AgentAction::LeaseAcquired {
                run_id: "r1".to_string(),
                intent: AgentLeaseIntent::Reply,
                generation: 18,
                action_request_id,
            },
            false,
        );
        assert!(matches!(
            effects.as_slice(),
            [AgentEffect::ReleaseLease {
                run_id,
                lease_generation: 18,
                ..
            }] if run_id == "r1"
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, AgentEffect::Reply { .. }))
        );
        assert_eq!(
            state.reply_drafts.get("r1").map(Editor::text),
            Some("keep this draft")
        );
        assert!(
            !state
                .run_by_id("r1")
                .is_some_and(|run| run.lease.owned_by_client)
        );
    }

    #[test]
    fn foreground_permission_arrival_releases_inline_permission_focus() {
        let mut permission_run = run("r1", "auth");
        permission_run.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run tests".to_string(),
            detail: String::new(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: "Allow once".to_string(),
            }],
            requires_inspector: false,
        });
        permission_run.lease = AgentLeaseView {
            generation: 14,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![permission_run]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char(' ')), false);
        let _ = state.step(press(KeyCode::F(2)), false);
        let _ = state.step(press(KeyCode::Down), false);
        assert!(matches!(state.surface, AgentSurface::Permission { .. }));
        assert!(matches!(
            state.foreground_permission_arrived().as_slice(),
            [AgentEffect::ReleaseLease {
                run_id,
                lease_generation: 14,
                ..
            }] if run_id == "r1"
        ));
        assert!(matches!(state.surface, AgentSurface::Peek));
    }

    #[test]
    fn leaving_inline_permission_focus_releases_its_transient_lease() {
        let mut permission_run = run("r1", "auth");
        permission_run.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run tests".to_string(),
            detail: String::new(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: "Allow once".to_string(),
            }],
            requires_inspector: false,
        });
        permission_run.lease = AgentLeaseView {
            generation: 15,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![permission_run]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char(' ')), false);
        let _ = state.step(press(KeyCode::F(2)), false);
        assert!(matches!(state.surface, AgentSurface::Permission { .. }));

        assert!(matches!(
            state.step(press(KeyCode::Esc), false).as_slice(),
            [AgentEffect::ReleaseLease {
                run_id,
                lease_generation: 15,
                ..
            }] if run_id == "r1"
        ));
        assert!(matches!(state.surface, AgentSurface::Peek));
        assert!(
            !state
                .run_by_id("r1")
                .is_some_and(|run| run.lease.owned_by_client)
        );
    }

    #[test]
    fn takeover_requires_a_deliberate_selection_and_names_the_run() {
        let mut owned = run("r1", "release audit");
        owned.lease = AgentLeaseView {
            generation: 8,
            owner: Some("terminal-b".to_string()),
            owned_by_client: false,
        };
        let mut state = state_with(vec![owned]);
        state.toggle();
        assert!(state.step(press(KeyCode::Char('t')), false).is_empty());
        assert!(matches!(
            state.surface,
            AgentSurface::TakeoverConfirm {
                ref run_id,
                selected: false
            } if run_id == "r1"
        ));
        assert!(state.step(press(KeyCode::Enter), false).is_empty());
        let _ = state.step(press(KeyCode::Char(' ')), false);
        assert!(matches!(
            state.step(press(KeyCode::Enter), false).as_slice(),
            [AgentEffect::Takeover {
                run_id,
                action_request_id
            }] if run_id == "r1" && action_request_id.starts_with("client-a:")
        ));
    }

    #[test]
    fn standalone_exit_requires_an_explicit_draft_disposition() {
        let mut reply_run = run("r1", "docs");
        reply_run.lease = AgentLeaseView {
            generation: 3,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = AgentDeckState::new(AgentDeckMode::Standalone, "client-a");
        let _ = state.replace_snapshot(AgentDeckSnapshot {
            sequence: 1,
            runs: vec![reply_run],
            new_run_target: None,
        });
        let _ = state.step(press(KeyCode::Char('r')), false);
        let _ = state.step(press(KeyCode::Char('x')), false);
        let _ = state.step(press(KeyCode::Esc), false);
        let _ = state.step(press(KeyCode::Char(' ')), false);
        assert!(state.step(press(KeyCode::Esc), false).is_empty());
        assert!(matches!(
            state.surface,
            AgentSurface::ExitConfirm { selected: None }
        ));
        assert!(state.step(press(KeyCode::Enter), false).is_empty());
        let _ = state.step(press(KeyCode::Char('2')), false);
        assert_eq!(
            state.step(press(KeyCode::Enter), false),
            vec![AgentEffect::ExitStandalone]
        );
        assert!(!state.has_unsent_drafts());
    }

    #[test]
    fn stale_history_after_detach_does_not_reopen_the_inspector() {
        let history_run = run("r1", "history");
        let history = AgentHistorySnapshot {
            run: history_run.clone(),
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        };
        let mut state = state_with(vec![history_run]);
        state.expect_inspector_history("r1");
        state.replace_history(history.clone());
        assert!(state.is_inspector());
        assert!(matches!(
            state.step(press(KeyCode::F(5)), false).as_slice(),
            [AgentEffect::Detach { run_id, .. }] if run_id == "r1"
        ));
        state.replace_history(history);
        assert!(!state.is_inspector());
    }

    #[test]
    fn declined_in_flight_history_clears_expectation_and_detaches_exact_lease() {
        let mut history_run = run("r1", "history");
        history_run.lease = AgentLeaseView {
            generation: 21,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let history = AgentHistorySnapshot {
            run: history_run.clone(),
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        };
        let mut state = state_with(vec![history_run]);
        state.expect_inspector_history("r1");
        assert!(matches!(
            state.decline_inspector_history(&history).as_slice(),
            [AgentEffect::Detach {
                run_id,
                lease_generation: 21,
                ..
            }] if run_id == "r1"
        ));
        assert!(!state.replace_history(history));
        assert!(!state.is_inspector());
    }

    #[test]
    fn foreground_permission_clears_attached_background_permission_focus() {
        let mut attached = run("r1", "auth");
        attached.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run tests".to_string(),
            detail: String::new(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: "Allow once".to_string(),
            }],
            requires_inspector: true,
        });
        attached.lease = AgentLeaseView {
            generation: 9,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![attached.clone()]);
        state.expect_inspector_history("r1");
        state.replace_history(AgentHistorySnapshot {
            run: attached,
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        });
        let _ = state.step(press(KeyCode::F(2)), false);
        let _ = state.step(press(KeyCode::Down), false);
        assert!(state.step(press(KeyCode::Enter), true).is_empty());
        let AgentSurface::Inspector(inspector) = &state.surface else {
            return;
        };
        assert!(!inspector.permission_focused);
        assert!(inspector.permission_selected.is_none());
    }

    #[test]
    fn long_inline_permission_escalates_to_retained_inspector() {
        let mut permission_run = run("r1", "auth");
        permission_run.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run command".to_string(),
            detail: String::new(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: "Allow this unusually detailed permission choice after reviewing every retained line"
                    .to_string(),
            }],
            requires_inspector: false,
        });
        permission_run.lease = AgentLeaseView {
            generation: 4,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![permission_run]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char(' ')), false);
        assert!(matches!(
            state.step(press(KeyCode::F(2)), false).as_slice(),
            [AgentEffect::Attach { run_id, .. }] if run_id == "r1"
        ));
    }

    #[test]
    fn attached_long_permission_can_scroll_to_its_complete_tail() -> io::Result<()> {
        let mut attached = run("r1", "auth");
        attached.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Review complete command context".to_string(),
            detail: "cwd: /repo\ncommand: cargo nextest run --all-features".to_string(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: format!("{} TAILMARK", "long choice ".repeat(80)),
            }],
            requires_inspector: true,
        });
        attached.lease = AgentLeaseView {
            generation: 9,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![attached.clone()]);
        state.expect_inspector_history("r1");
        state.replace_history(AgentHistorySnapshot {
            run: attached,
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        });
        let _ = state.step(press(KeyCode::F(2)), false);
        let _ = state.step(press(KeyCode::End), false);
        let backend = TestBackend::new(40, 16);
        let mut terminal = Terminal::new(backend)?;
        let _ = terminal.draw(|frame| state.render_inspector(frame, false))?;
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        assert!(rendered.contains("TAILMARK"), "{rendered}");
        Ok(())
    }

    #[test]
    fn history_gap_requests_resync_and_keeps_existing_events() {
        let history_run = run("r1", "history");
        let mut state = state_with(vec![history_run.clone()]);
        state.expect_inspector_history("r1");
        state.replace_history(AgentHistorySnapshot {
            run: history_run,
            first_retained_seq: 4,
            snapshot_seq: 5,
            history_complete: false,
            events: vec![AgentHistoryEvent {
                seq: 5,
                kind: AgentHistoryKind::Assistant,
                text: "retained".to_string(),
            }],
        });
        let effects = state.apply_history_event(AgentHistoryEvent {
            seq: 7,
            kind: AgentHistoryKind::Status,
            text: "gap".to_string(),
        });
        assert_eq!(
            effects,
            vec![AgentEffect::Resync {
                run_id: "r1".to_string(),
                expected_seq: 6,
                received_seq: 7,
            }]
        );
        let AgentSurface::Inspector(inspector) = &state.surface else {
            return;
        };
        assert_eq!(inspector.events.len(), 1);
        assert!(
            inspector
                .content()
                .contains("Earlier activity is not retained")
        );
    }

    #[test]
    fn retained_stream_chunks_coalesce_only_in_the_visible_projection() {
        let history_run = run("r1", "history");
        let mut state = state_with(vec![history_run.clone()]);
        state.expect_inspector_history("r1");
        state.replace_history(AgentHistorySnapshot {
            run: history_run,
            first_retained_seq: 1,
            snapshot_seq: 6,
            history_complete: true,
            events: vec![
                AgentHistoryEvent {
                    seq: 1,
                    kind: AgentHistoryKind::Assistant,
                    text: "hel".to_string(),
                },
                AgentHistoryEvent {
                    seq: 2,
                    kind: AgentHistoryKind::Assistant,
                    text: "lo".to_string(),
                },
                AgentHistoryEvent {
                    seq: 3,
                    kind: AgentHistoryKind::User,
                    text: "separate".to_string(),
                },
                AgentHistoryEvent {
                    seq: 4,
                    kind: AgentHistoryKind::Assistant,
                    text: "again".to_string(),
                },
                AgentHistoryEvent {
                    seq: 5,
                    kind: AgentHistoryKind::Thought,
                    text: "rea".to_string(),
                },
                AgentHistoryEvent {
                    seq: 6,
                    kind: AgentHistoryKind::Thought,
                    text: "son".to_string(),
                },
            ],
        });
        let AgentSurface::Inspector(inspector) = &state.surface else {
            return;
        };
        let content = inspector.content();
        assert_eq!(inspector.events.len(), 6);
        assert!(content.contains("hello"), "{content}");
        assert!(!content.contains("hel\nlo"), "{content}");
        assert!(content.contains("reason"), "{content}");
        assert_eq!(content.matches("Assistant ·").count(), 2);
        assert!(first_history_match(&content, "hello").is_some());
        assert_eq!(history_match_count(&content, "hello"), 1);
        assert!(matches!(
            state.command(AgentDeckCommand::Copy, false)
            .as_slice(),
            [AgentEffect::Copy { text }] if text.contains("hello")
        ));
    }

    #[test]
    fn initial_inspector_tail_fills_the_viewport_instead_of_scrolling_past_content()
    -> io::Result<()> {
        let history_run = run("r1", "history");
        let mut state = state_with(vec![history_run.clone()]);
        state.expect_inspector_history("r1");
        assert!(state.replace_history(AgentHistorySnapshot {
            run: history_run,
            first_retained_seq: 1,
            snapshot_seq: 2,
            history_complete: true,
            events: vec![
                AgentHistoryEvent {
                    seq: 1,
                    kind: AgentHistoryKind::Assistant,
                    text: "BACKGROUND_PRIVATE_OUTPUT".to_string(),
                },
                AgentHistoryEvent {
                    seq: 2,
                    kind: AgentHistoryKind::Status,
                    text: "Ready".to_string(),
                },
            ],
        }));
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| state.render_inspector(frame, false))?;
        let rendered = grid(terminal.backend());
        assert!(rendered.contains("BACKGROUND_PRIVATE_OUTPUT"), "{rendered}");
        assert!(rendered.contains("Ready"), "{rendered}");
        Ok(())
    }

    #[test]
    fn peek_and_inspector_preserve_exact_supervisor_metadata() -> io::Result<()> {
        let mut metadata_run = run("r1", "release-audit");
        metadata_run.native_session_id = Some("native-session-精确-42".to_string());
        metadata_run.confirmed_route = Some("openai/gpt-exact".to_string());
        metadata_run.attributed_cost = Some("USD 1.250000 (router)".to_string());
        let mut state = state_with(vec![metadata_run.clone()]);
        state.toggle();
        let _ = state.step(press(KeyCode::Char(' ')), false);

        let mut terminal = Terminal::new(TestBackend::new(120, 8))?;
        terminal.draw(|frame| state.render_peek(frame, frame.area(), false))?;
        let peek = grid(terminal.backend());
        let compact_peek = peek.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(compact_peek.contains("Agent: codex"), "{peek}");
        assert!(
            compact_peek.contains("Native session: native-session-精确-42"),
            "{peek}"
        );
        assert!(
            compact_peek.contains("Confirmed route: openai/gpt-exact"),
            "{peek}"
        );
        assert!(
            compact_peek.contains("Attributed cost: USD 1.250000 (router)"),
            "{peek}"
        );

        state.expect_inspector_history("r1");
        assert!(state.replace_history(AgentHistorySnapshot {
            run: metadata_run,
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        }));
        let AgentSurface::Inspector(inspector) = &state.surface else {
            return Ok(());
        };
        let content = inspector.content();
        assert!(content.contains("Agent: codex"), "{content}");
        assert!(
            content.contains("Native session: native-session-精确-42"),
            "{content}"
        );
        assert!(
            content.contains("Confirmed route: openai/gpt-exact"),
            "{content}"
        );
        assert!(
            content.contains("Attributed cost: USD 1.250000 (router)"),
            "{content}"
        );

        let unknown = AgentInspector {
            run: run("r2", "unknown"),
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
            scroll: 0,
            search: String::new(),
            searching: false,
            permission_focused: false,
            permission_selected: None,
            permission_scroll: 0,
        }
        .content();
        assert!(
            unknown.contains("Native session: not reported"),
            "{unknown}"
        );
        assert!(
            unknown.contains("Confirmed route: unknown (not reported)"),
            "{unknown}"
        );
        assert!(
            unknown.contains("Attributed cost: unknown (no attribution reported)"),
            "{unknown}"
        );
        Ok(())
    }

    #[test]
    fn new_run_target_fields_accept_manual_unicode_input() {
        let mut state = AgentDeckState::new(AgentDeckMode::Inline, "client-a");
        let _ = state.replace_snapshot(AgentDeckSnapshot::default());
        state.toggle();
        let _ = state.step(press(KeyCode::Char('n')), false);
        let _ = state.step(press(KeyCode::Tab), false);
        let _ = state.step(AgentAction::Event(Event::Paste("codex".to_string())), false);
        let _ = state.step(press(KeyCode::Tab), false);
        let _ = state.step(
            AgentAction::Event(Event::Paste("/tmp/工作树🧭".to_string())),
            false,
        );
        let _ = state.step(press(KeyCode::Tab), false);
        let _ = state.step(
            AgentAction::Event(Event::Paste("bitrouter/auto".to_string())),
            false,
        );
        let _ = state.step(press(KeyCode::Tab), false);
        let _ = state.step(
            AgentAction::Event(Event::Paste("run focused tests".to_string())),
            false,
        );
        let effects = state.step(press(KeyCode::Enter), false);
        assert!(matches!(
            effects.as_slice(),
            [AgentEffect::NewRun { target, prompt, .. }]
                if target.agent == "codex"
                    && target.directory == "/tmp/工作树🧭"
                    && target.route.as_deref() == Some("bitrouter/auto")
                    && prompt == "run focused tests"
        ));
    }

    #[test]
    fn active_new_run_target_field_stays_visible_on_a_narrow_deck() -> io::Result<()> {
        let directory = format!("/very/long/前置/{}ACTIVE_DIR_TAIL", "segment/".repeat(8));
        let route = format!("provider/{}ROUTE_TAIL", "model-segment-".repeat(6));
        let mut state = AgentDeckState::new(AgentDeckMode::Inline, "client-a");
        state.set_new_run_choices(NewAgentRunChoices {
            agents: vec!["codex".to_string()],
            directories: vec![NewAgentDirectoryChoice {
                directory,
                conflict: None,
            }],
            routes: vec![Some(route)],
        });
        state.toggle();
        let _ = state.step(press(KeyCode::Char('n')), false);
        let _ = state.step(press(KeyCode::Tab), false);
        let _ = state.step(press(KeyCode::Tab), false);

        let mut terminal = Terminal::new(TestBackend::new(40, 6))?;
        let mut directory_cursor = None;
        terminal.draw(|frame| {
            directory_cursor = state.render_expanded(frame, frame.area(), 3, false);
        })?;
        let directory_view = grid(terminal.backend());
        assert!(
            directory_view.contains("editing directory"),
            "{directory_view}"
        );
        assert!(
            directory_view.contains("ACTIVE_DIR_TAIL"),
            "{directory_view}"
        );
        assert!(directory_cursor.is_some_and(|cursor| cursor.y == 3));

        let _ = state.step(press(KeyCode::Tab), false);
        let mut route_cursor = None;
        terminal.draw(|frame| {
            route_cursor = state.render_expanded(frame, frame.area(), 3, false);
        })?;
        let route_view = grid(terminal.backend());
        assert!(route_view.contains("editing route"), "{route_view}");
        assert!(route_view.contains("ROUTE_TAIL"), "{route_view}");
        assert!(route_cursor.is_some_and(|cursor| cursor.y == 3));
        Ok(())
    }

    #[test]
    fn validated_new_run_choices_override_an_unchecked_snapshot_default() {
        let mut state = state_with(Vec::new());
        state.set_new_run_choices(NewAgentRunChoices {
            agents: vec!["codex".to_string()],
            directories: vec![NewAgentDirectoryChoice {
                directory: "/repo-worktree".to_string(),
                conflict: Some("claimed by foreground".to_string()),
            }],
            routes: vec![None],
        });
        state.toggle();
        let _ = state.step(press(KeyCode::Char('n')), false);
        assert!(
            state
                .new_run_target
                .as_ref()
                .and_then(|target| target.conflict.as_deref())
                .is_some_and(|conflict| conflict == "claimed by foreground")
        );
    }

    #[test]
    fn attached_permission_requires_fresh_focus_and_selection() {
        let mut attached = run("r1", "auth");
        attached.attention = AgentAttention::Permission(AgentPermissionView {
            permission_id: "perm-1".to_string(),
            title: "Run tests".to_string(),
            detail: String::new(),
            options: vec![AgentPermissionOption {
                id: "once".to_string(),
                label: "Allow once".to_string(),
            }],
            requires_inspector: true,
        });
        attached.lease = AgentLeaseView {
            generation: 9,
            owner: Some("client-a".to_string()),
            owned_by_client: true,
        };
        let mut state = state_with(vec![attached.clone()]);
        state.expect_inspector_history("r1");
        state.replace_history(AgentHistorySnapshot {
            run: attached,
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        });
        let _ = state.step(press(KeyCode::F(2)), false);
        assert!(state.step(press(KeyCode::Enter), false).is_empty());
        let _ = state.step(press(KeyCode::Down), false);
        assert!(matches!(
            state.step(press(KeyCode::Enter), false).as_slice(),
            [AgentEffect::RespondPermission {
                run_id,
                permission_id,
                option_id,
                lease_generation: 9,
                ..
            }] if run_id == "r1" && permission_id == "perm-1" && option_id == "once"
        ));
    }

    #[test]
    fn foreground_and_stopped_runs_are_hidden_from_default_rows() {
        let foreground = run("foreground", "foreground");
        let mut stopped = run("stopped", "stopped");
        stopped.process = AgentProcessState::Stopped;
        let mut state = state_with(vec![foreground, stopped]);
        state.set_foreground_run_id(Some("foreground".to_string()));
        assert!(!state.has_background_runs());
        state.toggle();
        assert!(state.ordered_runs().is_empty());
        let _ = state.step(press(KeyCode::Char('x')), false);
        assert_eq!(state.ordered_runs().len(), 1);
    }

    #[test]
    fn expanded_deck_renders_within_six_rows_at_minimum_viewport() -> io::Result<()> {
        let mut cjk = run("r1", "文档🧭");
        cjk.attention = AgentAttention::Question {
            question_id: "q1".to_string(),
            title: "同步中文？".to_string(),
            detail: "Should I update 中文 and keep sourceHash?".to_string(),
        };
        let mut state = state_with(vec![cjk]);
        state.toggle();
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend)?;
        let _ = terminal.draw(|frame| {
            let cursor = state.render_expanded(frame, frame.area(), 3, false);
            if let Some(position) = cursor {
                frame.set_cursor_position(position);
            }
        })?;
        assert_eq!(terminal.backend().buffer().area.height, 6);
        Ok(())
    }
}
