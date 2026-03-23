pub mod a2a;
pub mod admin;
pub mod admin_agents;
pub mod admin_tools;
pub mod agents;
#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "google")]
pub mod google;
pub mod mcp;
pub mod models;
#[cfg(feature = "openai")]
pub mod openai;
pub mod routes;
pub mod skills;
pub mod tools;

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
mod observe_ctx {
    use std::sync::Arc;
    use std::time::Instant;

    use bitrouter_core::observe::{CallerContext, ObserveCallback};

    /// Bundles observation-related context passed through streaming handlers.
    ///
    /// Created at the call site and consumed inside `handle_stream_with_observe`
    /// to emit success/failure observation events after the stream completes.
    pub(crate) struct StreamObserveContext {
        pub observer: Arc<dyn ObserveCallback>,
        pub route: String,
        pub provider: String,
        pub target_model: String,
        pub caller: CallerContext,
        pub start: Instant,
    }
}

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
pub(crate) use observe_ctx::StreamObserveContext;
