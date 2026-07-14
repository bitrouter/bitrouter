//! Pure event + effect types for the TUI reducer.
//!
//! `AppEvent` is what the reducer folds. `Effect` is what the reducer *returns*
//! for the event loop to execute against live sessions (Elm-style: `reduce` stays
//! pure; side effects happen in the loop). `Incoming` is the richer channel
//! message the loop receives (it may carry non-pure handles); the loop converts
//! each `Incoming` into an `AppEvent` before reducing.

use crate::risk::Risk;
use agent_client_protocol::schema::v1::StopReason;
use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind};
use bitrouter_substrate::up::PendingPermission;
use crossterm::event::KeyEvent;

/// A permission option offered to the user, reduced to display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermOption {
    pub outcome: PermissionOutcome,
    pub label: String,
}

/// A structured file diff (old → new text) carried by a permission request or
/// tool call, ready for the line-diff renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffData {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// Pure event the reducer folds into `AppState`.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A key was pressed.
    Key(KeyEvent),
    /// A session emitted a translated update.
    Update {
        record_id: String,
        update: SessionUpdateKind,
    },
    /// A session is requesting permission (display data only; the resolvable
    /// handle lives in the loop, keyed by `record_id`).
    Permission {
        record_id: String,
        title: String,
        diff: Option<DiffData>,
        options: Vec<PermOption>,
        /// Classified by the loop from the tool call's kind + locations.
        risk: Risk,
    },
    /// A prompt turn completed; carries the typed ACP stop reason. Feeds the
    /// working/idle state and (with a non-empty diff) the review queue.
    TurnEnded {
        record_id: String,
        stop_reason: StopReason,
    },
    /// The session's agent child exited.
    Exited { record_id: String },
    /// A newly launched session's pane should appear.
    AgentSpawned {
        record_id: String,
        agent_id: String,
        /// The fleet-allocated `PORT` (shown in the roster row), if any.
        port: Option<u16>,
    },
    /// Launching a session failed; surface a transient notice.
    AgentSpawnFailed { agent_id: String, error: String },
    /// A submitted prompt failed to reach the agent; surface it in the pane
    /// (otherwise a dead proxy/agent looks like a silent hang).
    PromptFailed { record_id: String, error: String },
    /// The loop checked a finished turn's worktree: non-empty diff (and
    /// passing checks) — the agent is ready to review.
    ReviewReady {
        record_id: String,
        files: u64,
        adds: u64,
        dels: u64,
    },
    /// The configured verification checks failed in the agent's worktree.
    /// The failure loops back to the subagent (not the human) while retries
    /// remain; then it surfaces for review with a warning.
    ChecksFailed { record_id: String, output: String },
    /// The agent's full worktree diff, loaded for in-pane review (`D`).
    DiffLoaded { record_id: String, text: String },
    /// A loop-side git operation (merge/apply) finished; surface the outcome
    /// in the pane. Success clears the review flag.
    OpDone {
        record_id: String,
        message: String,
        ok: bool,
    },
    /// Periodic UI tick (drives the running-agent spinner animation).
    Tick,
}

/// Side effect the loop performs after a reduce. Keeps `reduce` pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Send `text` as a prompt to the session `record_id`.
    Prompt { record_id: String, text: String },
    /// Resolve the pending permission for `record_id` with `outcome`.
    ResolvePermission {
        record_id: String,
        outcome: PermissionOutcome,
    },
    /// Tear down and exit the TUI.
    Quit,
    /// Ring the terminal bell (background pane needs attention).
    Bell,
    /// Launch a new agent session (the loop performs the async launch).
    SpawnAgent { agent_id: String },
    /// Shut down and remove the session `record_id`.
    CloseAgent { record_id: String },
    /// A turn ended cleanly: inspect the agent's worktree (diff + checks) and
    /// report back with `ReviewReady`/`ChecksFailed`.
    CheckReview { record_id: String },
    /// Load the agent's full worktree diff for in-pane review.
    LoadDiff { record_id: String },
    /// Merge the agent's branch into the base repo (human-driven; requires
    /// committed work). Serialized: the loop runs one integration at a time.
    Merge { record_id: String },
    /// Apply the agent's diff onto the base working tree, uncommitted.
    Apply { record_id: String },
}

/// The channel message the loop receives. Carries the real `PendingPermission`
/// handle (not `Clone`), which the loop stashes before deriving `AppEvent`.
pub enum Incoming {
    Update {
        record_id: String,
        update: SessionUpdateKind,
    },
    Permission {
        record_id: String,
        pending: Box<PendingPermission>,
    },
    TurnEnded {
        record_id: String,
        stop_reason: StopReason,
    },
    Exited {
        record_id: String,
    },
    PromptFailed {
        record_id: String,
        error: String,
    },
    ReviewReady {
        record_id: String,
        files: u64,
        adds: u64,
        dels: u64,
    },
    ChecksFailed {
        record_id: String,
        output: String,
    },
    DiffLoaded {
        record_id: String,
        text: String,
    },
}
