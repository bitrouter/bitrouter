//! Unified spend log storage trait.
//!
//! A single [`SpendLog`] type and [`SpendStore`] trait serve all service types
//! (model, tool, agent), distinguished by [`ServiceType`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use chrono::NaiveDateTime;
use uuid::Uuid;

/// Discriminator for the service that produced a spend log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    Model,
    Tool,
    Agent,
}

impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model => f.write_str("model"),
            Self::Tool => f.write_str("tool"),
            Self::Agent => f.write_str("agent"),
        }
    }
}

/// A single spend log entry representing one completed request, tool call,
/// or agent call.
#[derive(Debug, Clone)]
pub struct SpendLog {
    pub id: Uuid,
    /// Which service type produced this log entry.
    pub service_type: ServiceType,
    pub account_id: Option<String>,
    pub session_id: Option<Uuid>,
    /// For Model: route name. For Tool: server name. For Agent: agent name.
    pub service_name: String,
    /// For Model: `"provider:model_id"`. For Tool: tool name. For Agent: A2A method.
    pub operation: String,
    /// Token counts — 0 for tool/agent service types.
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost: f64,
    pub latency_ms: u64,
    pub success: bool,
    /// Error variant name (model) or error message (tool/agent).
    pub error_info: Option<String>,
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
    /// When `service_type` is `None`, returns the total across all service types.
    fn query_total_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
        service_type: Option<ServiceType>,
    ) -> Pin<Box<dyn Future<Output = f64> + Send + '_>>;
}
