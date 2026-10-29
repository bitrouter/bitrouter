//! Durable conversation and input contracts. Transports supply authenticated
//! callers and never select a workspace or permission grant on their behalf.

use bitrouter_ai::harness::thread::{PermissionProfile, ThreadSnapshot};
use std::path::PathBuf;

use crate::gateway::caller::CallerContext;
use serde::{Deserialize, Serialize};

use bitrouter_ai::harness::agent::AgentConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadTarget {
    pub thread_id: String,
    pub server_instance_id: String,
}

/// A completed close result. Replays must not retire newer attachments or work.
#[derive(Debug, Clone)]
pub struct ThreadCloseReceipt {
    pub snapshot: ThreadSnapshot,
    pub replayed: bool,
}

pub struct ThreadHistoryRequest {
    pub after: u64,
    pub cutoff: Option<u64>,
    pub limit: usize,
}

pub struct ThreadRequest {
    pub caller: CallerContext,
    pub workspace: PathBuf,
    pub config: AgentConfig,
    pub permission_profile: PermissionProfile,
    pub verification_command: Option<String>,
    pub idempotency_key: String,
}

/// Trusted host configuration; this is not accepted from an input RPC.
pub struct WorkspaceGrant {
    pub workspace: PathBuf,
    pub permission_profiles: Vec<PermissionProfile>,
}
