//! Composite observer that fans out events to multiple callbacks.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bitrouter_core::observe::{ObserveCallback, RequestFailureEvent, RequestSuccessEvent};

/// An [`ObserveCallback`] that delegates to multiple inner callbacks.
///
/// Events are dispatched sequentially to each callback. Since callbacks
/// should be fast and infallible from the caller's perspective, sequential
/// dispatch avoids the overhead of spawning concurrent tasks.
pub struct CompositeObserver {
    callbacks: Vec<Arc<dyn ObserveCallback>>,
}

impl CompositeObserver {
    pub fn new(callbacks: Vec<Arc<dyn ObserveCallback>>) -> Self {
        Self { callbacks }
    }
}

impl ObserveCallback for CompositeObserver {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.callbacks {
                cb.on_request_success(event.clone()).await;
            }
        })
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            for cb in &self.callbacks {
                cb.on_request_failure(event.clone()).await;
            }
        })
    }
}
