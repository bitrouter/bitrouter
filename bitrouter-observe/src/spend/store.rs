//! Spend log storage trait.

use std::future::Future;
use std::pin::Pin;

use chrono::NaiveDateTime;
use uuid::Uuid;

/// A single spend log entry representing one completed request.
#[derive(Debug, Clone)]
pub struct SpendLog {
    pub id: Uuid,
    pub account_id: Option<String>,
    pub session_id: Option<Uuid>,
    pub model: String,
    pub provider: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost: f64,
    pub latency_ms: u64,
    pub success: bool,
    pub error_type: Option<String>,
    pub created_at: NaiveDateTime,
}

/// Trait for persisting spend logs.
///
/// Implementations must be infallible from the caller's perspective — errors
/// should be logged internally and swallowed. Spend logging must never break
/// request serving.
pub trait SpendStore: Send + Sync {
    /// Writes a spend log entry to the store.
    fn write(&self, log: SpendLog) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Returns total cost (USD) for an account since the given timestamp.
    ///
    /// When `since` is `None`, returns the all-time total.
    fn query_total_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
    ) -> Pin<Box<dyn Future<Output = f64> + Send + '_>>;
}
