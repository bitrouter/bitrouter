//! Pure event + effect types for the TUI reducer.
//!
//! `AppEvent` is what the reducer folds. `Effect` is what the reducer *returns*
//! for the event loop to execute against live sessions (Elm-style: `reduce` stays
//! pure; side effects happen in the loop). `Incoming` is the richer channel
//! message the loop receives (it may carry non-pure handles); the loop converts
//! each `Incoming` into an `AppEvent` before reducing.

use bitrouter_substrate::translate::{PermissionOutcome, SessionUpdateKind};
use bitrouter_substrate::up::PendingPermission;
use crossterm::event::KeyEvent;

/// A permission option offered to the user, reduced to display data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermOption {
    pub outcome: PermissionOutcome,
    pub label: String,
}

/// Deterministic risk classification of a permission request, computed by the
/// loop from the tool call's structured fields (kind + locations). The reducer
/// combines it with the pane's autonomy level to decide surface vs auto-allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads/searches and writes confined to the project tree.
    Low,
    /// Deletes, command execution, network access, writes outside the project
    /// tree, or anything unclassifiable (conservative default).
    High,
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
        diff: Option<String>,
        options: Vec<PermOption>,
        /// Classified by the loop from the tool call's kind + locations.
        risk: Risk,
    },
    /// The session's agent child exited.
    Exited { record_id: String },
    /// A newly launched session's pane should appear.
    AgentSpawned { record_id: String, agent_id: String },
    /// Launching a session failed; surface a transient notice.
    AgentSpawnFailed { agent_id: String, error: String },
    /// A submitted prompt failed to reach the agent; surface it in the pane
    /// (otherwise a dead proxy/agent looks like a silent hang).
    PromptFailed { record_id: String, error: String },
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
    Exited {
        record_id: String,
    },
    PromptFailed {
        record_id: String,
        error: String,
    },
}
