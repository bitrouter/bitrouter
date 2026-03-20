//! Tool spend log storage trait.

use std::future::Future;
use std::pin::Pin;

use chrono::NaiveDateTime;
use uuid::Uuid;

/// A single tool spend log entry representing one completed tool call.
#[derive(Debug, Clone)]
pub struct ToolSpendLog {
    pub id: Uuid,
    pub account_id: Option<String>,
    pub server: String,
    pub tool: String,
    pub cost: f64,
    pub latency_ms: u64,
    pub success: bool,
    pub error_message: Option<String>,
    pub created_at: NaiveDateTime,
}

/// Trait for persisting tool spend logs.
///
/// Implementations must be infallible from the caller's perspective — errors
/// should be logged internally and swallowed. Spend logging must never break
/// request serving.
pub trait ToolSpendStore: Send + Sync {
    /// Writes a tool spend log entry to the store.
    fn write(&self, log: ToolSpendLog) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Returns total tool cost (USD) for an account since the given timestamp.
    ///
    /// When `since` is `None`, returns the all-time total.
    fn query_tool_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
    ) -> Pin<Box<dyn Future<Output = f64> + Send + '_>>;
}
