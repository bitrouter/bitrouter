//! Observation context and helpers for A2A gateway instrumentation.

use std::sync::Arc;

use bitrouter_core::observe::{
    AgentCallFailureEvent, AgentCallSuccessEvent, AgentObserveCallback, AgentRequestContext,
    CallerContext,
};
use tokio::time::Instant;

/// Shared context threaded through A2A gateway filters for observation.
#[derive(Clone)]
pub(crate) struct A2aObserveContext {
    pub observer: Arc<dyn AgentObserveCallback>,
    pub caller: CallerContext,
}

/// Fire a success [`AgentCallSuccessEvent`] for a completed A2A operation.
///
/// The event is spawned as an async task so it never blocks the response path.
pub(crate) fn emit_agent_success(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
) {
    let Some(ctx) = ctx else { return };
    let event = AgentCallSuccessEvent {
        ctx: AgentRequestContext {
            agent: agent_name.to_string(),
            method: method.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_agent_call_success(event).await });
}

/// Fire a failure [`AgentCallFailureEvent`] from an error description.
pub(crate) fn emit_agent_failure(
    ctx: &Option<A2aObserveContext>,
    agent_name: &str,
    method: &str,
    start: Instant,
    error: &str,
) {
    let Some(ctx) = ctx else { return };
    let event = AgentCallFailureEvent {
        ctx: AgentRequestContext {
            agent: agent_name.to_string(),
            method: method.to_string(),
            caller: ctx.caller.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
        },
        error: error.to_string(),
    };
    let obs = ctx.observer.clone();
    tokio::spawn(async move { obs.on_agent_call_failure(event).await });
}
