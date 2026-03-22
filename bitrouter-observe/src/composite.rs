//! Composite observer that fans out events to multiple callbacks.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::observe::{
    AgentCallEvent, AgentObserveCallback, ObserveCallback, RequestFailureEvent,
    RequestSuccessEvent, ToolCallEvent, ToolObserveCallback,
};

/// An observer that delegates to multiple inner callbacks for all service types.
///
/// Events are dispatched sequentially to each callback. Since callbacks
/// should be fast and infallible from the caller's perspective, sequential
/// dispatch avoids the overhead of spawning concurrent tasks.
pub struct CompositeObserver {
    model_callbacks: Vec<Arc<dyn ObserveCallback>>,
    tool_callbacks: Vec<Arc<dyn ToolObserveCallback>>,
    agent_callbacks: Vec<Arc<dyn AgentObserveCallback>>,
}

impl CompositeObserver {
    pub fn new(
        model_callbacks: Vec<Arc<dyn ObserveCallback>>,
        tool_callbacks: Vec<Arc<dyn ToolObserveCallback>>,
        agent_callbacks: Vec<Arc<dyn AgentObserveCallback>>,
    ) -> Self {
        Self {
            model_callbacks,
            tool_callbacks,
            agent_callbacks,
        }
    }
}

impl ObserveCallback for CompositeObserver {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.model_callbacks {
                cb.on_request_success(event.clone()).await;
            }
        })
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.model_callbacks {
                cb.on_request_failure(event.clone()).await;
            }
        })
    }
}

impl ToolObserveCallback for CompositeObserver {
    fn on_tool_call(&self, event: ToolCallEvent) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.tool_callbacks {
                cb.on_tool_call(event.clone()).await;
            }
        })
    }
}

impl AgentObserveCallback for CompositeObserver {
    fn on_agent_call(
        &self,
        event: AgentCallEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.agent_callbacks {
                cb.on_agent_call(event.clone()).await;
            }
        })
    }
}
