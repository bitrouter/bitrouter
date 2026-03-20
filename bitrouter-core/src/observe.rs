//! Handler-level observation callback for request lifecycle events.
//!
//! [`ObserveCallback`] fires from the API handler layer with full request
//! context (route, provider, model, account, latency, usage/error).
//! This is complementary to [`GenerationHook`](crate::hooks::GenerationHook)
//! which fires at the model layer with no request context.

use std::future::Future;
use std::pin::Pin;

use crate::auth::claims::{BudgetRange, BudgetScope};
use crate::errors::BitrouterError;
use crate::models::language::usage::LanguageModelUsage;

/// Authenticated caller context extracted from JWT claims.
///
/// Carries the account identifier and any claim-based permissions (budget,
/// model allowlist) through the API handler layer. Constructed in the auth
/// filter and consumed by observers and (in the future) enforcement middleware.
#[derive(Debug, Clone, Default)]
pub struct CallerContext {
    /// The account that made the request, if authentication is enabled.
    pub account_id: Option<String>,
    /// Optional model-name patterns this caller may access.
    pub models: Option<Vec<String>>,
    /// Optional tool-name patterns this caller may access.
    pub tools: Option<Vec<String>>,
    /// Budget limit in micro USD.
    pub budget: Option<u64>,
    /// Whether the budget applies per-session or per-account.
    pub budget_scope: Option<BudgetScope>,
    /// The range over which the budget is measured.
    pub budget_range: Option<BudgetRange>,
}

/// Context about the request available to observation callbacks.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The incoming model name (route key).
    pub route: String,
    /// The resolved provider name.
    pub provider: String,
    /// The resolved model ID sent to the provider.
    pub model: String,
    /// Authenticated caller context (account ID, budget claims).
    pub caller: CallerContext,
    /// End-to-end request latency in milliseconds.
    pub latency_ms: u64,
}

/// Event emitted when a request completes successfully.
#[derive(Debug, Clone)]
pub struct RequestSuccessEvent {
    /// Request context.
    pub ctx: RequestContext,
    /// Token usage reported by the model.
    pub usage: LanguageModelUsage,
}

/// Event emitted when a request fails.
#[derive(Debug, Clone)]
pub struct RequestFailureEvent {
    /// Request context.
    pub ctx: RequestContext,
    /// The error that caused the failure.
    pub error: BitrouterError,
}

/// Callback trait for observing completed API requests.
///
/// Implementations receive rich, typed events with full request context
/// and can perform side effects such as spend logging, metrics aggregation,
/// or alerting. Errors in callbacks are expected to be handled internally
/// (e.g. logged and swallowed) — they must never break request serving.
///
/// Methods return boxed futures for dyn-compatibility, allowing multiple
/// observers to be composed via [`CompositeObserver`](see bitrouter-observe).
pub trait ObserveCallback: Send + Sync {
    /// Called after a request completes successfully.
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Called after a request fails.
    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

// ── MCP tool call observation ────────────────────────────────────────

/// Event emitted when an MCP tool call completes.
#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    /// The account that made the call, if authentication is enabled.
    pub account_id: Option<String>,
    /// The upstream MCP server name.
    pub server: String,
    /// The un-namespaced tool name.
    pub tool: String,
    /// Cost of this tool call in USD.
    pub cost: f64,
    /// Latency of the tool call in milliseconds.
    pub latency_ms: u64,
    /// Whether the tool call succeeded.
    pub success: bool,
    /// Error message if the call failed.
    pub error_message: Option<String>,
}

/// Callback trait for observing completed MCP tool calls.
///
/// Parallel to [`ObserveCallback`] but for tool invocations rather than
/// LLM requests. Implementations persist tool spend logs or emit metrics.
pub trait ToolObserveCallback: Send + Sync {
    /// Called after a tool call completes (success or failure).
    fn on_tool_call(&self, event: ToolCallEvent) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
