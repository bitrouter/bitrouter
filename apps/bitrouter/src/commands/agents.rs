//! Typed agent actions dispatched by shared application operations.

use crate::presentation::agents::NewAgentRunTarget;

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
