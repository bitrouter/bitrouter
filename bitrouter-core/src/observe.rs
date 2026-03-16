//! Handler-level observation callback for request lifecycle events.
//!
//! [`ObserveCallback`] fires from the API handler layer with full request
//! context (route, provider, model, account, latency, usage/error).
//! This is complementary to [`GenerationHook`](crate::hooks::GenerationHook)
//! which fires at the model layer with no request context.

use std::future::Future;
use std::pin::Pin;

use crate::errors::BitrouterError;
use crate::models::language::usage::LanguageModelUsage;

/// Context about the request available to observation callbacks.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The incoming model name (route key).
    pub route: String,
    /// The resolved provider name.
    pub provider: String,
    /// The resolved model ID sent to the provider.
    pub model: String,
    /// The account that made the request, if authentication is enabled.
    pub account_id: Option<String>,
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
