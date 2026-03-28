//! Observation context and helpers for MCP tool call instrumentation.

use std::sync::Arc;

use bitrouter_core::observe::{
    CallerContext, ToolCallFailureEvent, ToolCallSuccessEvent, ToolObserveCallback,
    ToolRequestContext,
};
use tokio::time::Instant;

/// Shared context threaded through MCP tool call handlers for observation.
#[derive(Clone)]
pub(crate) struct McpObserveContext {
    pub observer: Arc<dyn ToolObserveCallback>,
    pub caller: CallerContext,
}

/// Fire a success [`ToolCallSuccessEvent`] for a completed MCP tool call.
///
/// The event is spawned as an async task so it never blocks the response path.
pub(crate) fn emit_tool_success(
    ctx: &Option<McpObserveContext>,
    server: &str,
    tool: &str,
    start: Instant,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallSuccessEvent {
        ctx: ToolRequestContext {
            server: server.to_string(),
            tool: tool.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_success(event).await });
}

/// Fire a failure [`ToolCallFailureEvent`] for a failed MCP tool call.
///
/// The event is spawned as an async task so it never blocks the response path.
pub(crate) fn emit_tool_failure(
    ctx: &Option<McpObserveContext>,
    server: &str,
    tool: &str,
    start: Instant,
    error: &str,
) {
    let Some(ctx) = ctx else { return };
    let event = ToolCallFailureEvent {
        ctx: ToolRequestContext {
            server: server.to_string(),
            tool: tool.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
        error: error.to_string(),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_tool_call_failure(event).await });
}
