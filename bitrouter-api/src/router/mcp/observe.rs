//! Observation context and helpers for MCP tool call instrumentation.

use std::sync::Arc;

use bitrouter_core::observe::{ToolCallEvent, ToolObserveCallback};
use tokio::time::Instant;

/// Cost lookup function: `(server_name, tool_name) -> cost_usd`.
pub type ToolCostFn = Arc<dyn Fn(&str, &str) -> f64 + Send + Sync>;

/// Shared context threaded through MCP tool call handlers for observation.
#[derive(Clone)]
pub(crate) struct McpObserveContext {
    pub observer: Arc<dyn ToolObserveCallback>,
    pub cost_fn: ToolCostFn,
}

/// Fire a [`ToolCallEvent`] for a completed MCP tool call.
///
/// The event is spawned as an async task so it never blocks the response path.
/// Accepts `Result<(), String>` where the `Err` variant holds the error message.
pub(crate) fn emit_tool_event(
    ctx: &Option<McpObserveContext>,
    server: &str,
    tool: &str,
    start: Instant,
    result: &Result<(), String>,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallEvent {
        account_id: None,
        server: server.to_string(),
        tool: tool.to_string(),
        cost: (ctx.cost_fn)(server, tool),
        latency_ms: start.elapsed().as_millis() as u64,
        success: result.is_ok(),
        error_message: result.as_ref().err().cloned(),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call(event).await });
}
