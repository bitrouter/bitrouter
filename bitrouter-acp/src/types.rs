//! ACP-specific operational types.
//!
//! Protocol-neutral agent session types (events, permissions, stop reasons)
//! live in `bitrouter_core::agents::event`. This module retains types that
//! are specific to ACP agent discovery, installation, and subprocess
//! management.

use std::path::PathBuf;

use bitrouter_core::agents::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use tokio::sync::mpsc;

/// Commands sent to an agent's dedicated OS thread.
pub(crate) enum AgentCommand {
    /// Submit a prompt. The agent thread sends events to `reply_tx` for
    /// this turn, then drops it when the turn ends.
    Prompt {
        text: String,
        reply_tx: mpsc::Sender<AgentEvent>,
    },
    /// Resolve a pending permission request.
    RespondPermission {
        request_id: PermissionRequestId,
        response: PermissionResponse,
    },
    /// Graceful shutdown.
    Disconnect,
}

/// How an agent was discovered and how it should be launched.
#[derive(Debug, Clone)]
pub enum AgentAvailability {
    /// Binary found on PATH at this location.
    OnPath(PathBuf),
    /// Not on PATH, but can be launched or installed via distribution metadata.
    Distributable,
}

/// An agent discovered during startup (on PATH or via distribution metadata).
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub name: String,
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub availability: AgentAvailability,
}

/// Progress of a binary agent installation.
#[derive(Debug, Clone)]
pub enum InstallProgress {
    /// Downloading the archive.
    Downloading {
        bytes_received: u64,
        total: Option<u64>,
    },
    /// Extracting the archive.
    Extracting,
    /// Installation completed successfully.
    Done(PathBuf),
    /// Installation failed.
    Failed(String),
}

// Compile-time assertions: all public types must be Send.
const _: () = {
    const fn _assert<T: Send>() {}
    _assert::<AgentCommand>();
    _assert::<AgentAvailability>();
    _assert::<DiscoveredAgent>();
    _assert::<InstallProgress>();
};
