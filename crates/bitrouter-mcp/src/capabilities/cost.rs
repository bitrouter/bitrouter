//! The cost capability port: the `fleet_cost` tool's backing query. The
//! app-side adapter reads the local metering database; the crate stays
//! storage-agnostic and only owns the tool schema.

use crate::error::ToolError;

/// A point-in-time spend snapshot for the orchestrating session. Returns
/// pre-built JSON (today's spend, request count, all-time totals) — the crate
/// never touches the metering database itself.
#[async_trait::async_trait]
pub trait CostQuery: Send + Sync {
    /// The current spend snapshot.
    async fn snapshot(&self) -> Result<serde_json::Value, ToolError>;
}
