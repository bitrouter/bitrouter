//! Handler-level observation callback for request lifecycle events.
//!
//! [`ObserveCallback`] fires from the API handler layer with full request
//! context (route, provider, model, account, latency, usage/error).
//! This is complementary to [`GenerationHook`](crate::hooks::GenerationHook)
//! which fires at the model layer with no request context.

use std::future::Future;
use std::pin::Pin;

use crate::auth::claims::BudgetScope;
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
    /// Budget limit in micro USD.
    pub budget: Option<u64>,
    /// Whether the budget applies per-session or per-account.
    pub budget_scope: Option<BudgetScope>,
    /// OWS agent key for payment authorization (from JWT `key` claim).
    pub key: Option<String>,
    /// CAIP-2 chain identifier derived from the operator's `iss` CAIP-10.
    /// Used for MPP payment network selection.
    pub chain: Option<String>,
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

/// Context about a tool call request available to observation callbacks.
#[derive(Debug, Clone)]
pub struct ToolRequestContext {
    /// The upstream MCP server name.
    pub server: String,
    /// The un-namespaced tool name.
    pub tool: String,
    /// Authenticated caller context (account ID, tool allowlist, budget claims).
    pub caller: CallerContext,
    /// End-to-end tool call latency in milliseconds.
    pub latency_ms: u64,
}

/// Event emitted when an MCP tool call completes successfully.
#[derive(Debug, Clone)]
pub struct ToolCallSuccessEvent {
    /// Tool call context.
    pub ctx: ToolRequestContext,
}

/// Event emitted when an MCP tool call fails.
#[derive(Debug, Clone)]
pub struct ToolCallFailureEvent {
    /// Tool call context.
    pub ctx: ToolRequestContext,
    /// Error description.
    pub error: String,
}

/// Callback trait for observing completed MCP tool calls.
///
/// Parallel to [`ObserveCallback`] but for tool invocations rather than
/// LLM requests. Implementations persist tool spend logs or emit metrics.
pub trait ToolObserveCallback: Send + Sync {
    /// Called after a tool call completes successfully.
    fn on_tool_call_success(
        &self,
        event: ToolCallSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Called after a tool call fails.
    fn on_tool_call_failure(
        &self,
        event: ToolCallFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

// ── A2A agent call observation ───────────────────────────────────────

/// Context about an agent call request available to observation callbacks.
#[derive(Debug, Clone)]
pub struct AgentRequestContext {
    /// The upstream agent name.
    pub agent: String,
    /// The A2A method dispatched (e.g. `"message/send"`, `"tasks/get"`).
    pub method: String,
    /// Authenticated caller context.
    pub caller: CallerContext,
    /// End-to-end agent call latency in milliseconds.
    pub latency_ms: u64,
}

/// Event emitted when an A2A agent call completes successfully.
#[derive(Debug, Clone)]
pub struct AgentCallSuccessEvent {
    /// Agent call context.
    pub ctx: AgentRequestContext,
}

/// Event emitted when an A2A agent call fails.
#[derive(Debug, Clone)]
pub struct AgentCallFailureEvent {
    /// Agent call context.
    pub ctx: AgentRequestContext,
    /// Error description.
    pub error: String,
}

/// Callback trait for observing completed A2A agent calls.
///
/// Parallel to [`ToolObserveCallback`] but for agent invocations.
/// Implementations persist agent spend logs or emit metrics.
pub trait AgentObserveCallback: Send + Sync {
    /// Called after an agent call completes successfully.
    fn on_agent_call_success(
        &self,
        event: AgentCallSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Called after an agent call fails.
    fn on_agent_call_failure(
        &self,
        event: AgentCallFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
