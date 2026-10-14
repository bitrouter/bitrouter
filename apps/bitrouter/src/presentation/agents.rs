//! Agent inventory and history facts without terminal or service handles.

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

/// Presentation facts supplied by either ACP inventory or the BRO Thread client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: String,
    pub label: String,
    pub directory: String,
    pub status: String,
    pub needs_input: bool,
    pub working: bool,
    pub metadata: Vec<String>,
}
