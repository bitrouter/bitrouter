//! Context-window occupancy, as ACP reports it.
//!
//! ## Why this is the only token signal here
//!
//! ACP's **stable** usage signal is `session/update UsageUpdate`, which reports
//! context-window occupancy (`used`/`size` tokens, optional cumulative cost) —
//! not per-turn input/output deltas. Those exist only behind the schema
//! crate's `unstable_end_turn_token_usage` feature, which this workspace does
//! not enable, so there is nothing else to record.
//!
//! [`crate::acp::client::AcpClient`] writes the latest `UsageUpdate` into a
//! [`SharedContextUsage`] slot as it forwards one; a caller reads the slot
//! when it builds a turn record. Both halves used to live behind an
//! `ExecutionHook` on the ACP pipeline — the pipeline is gone, and latency and
//! stop reason turned out to be visible on the prompt round-trip itself, so
//! only the part ACP actually carries survives here.

use std::sync::{Arc, Mutex};

/// Context-window occupancy reported by the agent's latest `UsageUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    /// Tokens currently in context.
    pub used: u64,
    /// Total context-window size in tokens.
    pub size: u64,
}

/// Shared slot holding the latest [`ContextUsage`]; written by the client's
/// `session/update` handler, read by whoever builds a turn record.
pub type SharedContextUsage = Arc<Mutex<Option<ContextUsage>>>;
