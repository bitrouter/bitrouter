//! Observation context and helpers for A2A gateway instrumentation.

use std::sync::Arc;

use bitrouter_a2a::error::A2aGatewayError;
use bitrouter_core::observe::{AgentCallEvent, AgentObserveCallback};
use tokio::time::Instant;

/// Cost lookup function: `(agent_name, method) -> cost_usd`.
pub type AgentCostFn = Arc<dyn Fn(&str, &str) -> f64 + Send + Sync>;

/// Shared context threaded through A2A gateway filters for observation.
#[derive(Clone)]
pub(crate) struct A2aObserveContext {
    pub observer: Arc<dyn AgentObserveCallback>,
    pub cost_fn: AgentCostFn,
}

/// Fire an [`AgentCallEvent`] for a completed A2A operation.
///
/// The event is spawned as an async task so it never blocks the response path.
pub(crate) fn emit_agent_event<T>(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
    result: &Result<T, A2aGatewayError>,
) {
    let Some(ctx) = ctx else { return };
    let event = AgentCallEvent {
        account_id: None,
        agent: agent_name.to_string(),
        method: method.to_string(),
        cost: (ctx.cost_fn)(agent_name, method),
        latency_ms: start.elapsed().as_millis() as u64,
        success: result.is_ok(),
        error_message: result.as_ref().err().map(|e| e.to_string()),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_agent_call(event).await });
}

/// Fire a success [`AgentCallEvent`] (no error payload).
pub(crate) fn emit_agent_success(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
) {
    let Some(ctx) = ctx else { return };
    let event = AgentCallEvent {
        account_id: None,
        agent: agent_name.to_string(),
        method: method.to_string(),
        cost: (ctx.cost_fn)(agent_name, method),
        latency_ms: start.elapsed().as_millis() as u64,
        success: true,
        error_message: None,
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_agent_call(event).await });
}

/// Fire a failure [`AgentCallEvent`] from an error reference.
pub(crate) fn emit_agent_error(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
    error: &A2aGatewayError,
) {
    let Some(ctx) = ctx else { return };
    let event = AgentCallEvent {
        account_id: None,
        agent: agent_name.to_string(),
        method: method.to_string(),
        cost: (ctx.cost_fn)(agent_name, method),
        latency_ms: start.elapsed().as_millis() as u64,
        success: false,
        error_message: Some(error.to_string()),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_agent_call(event).await });
}
