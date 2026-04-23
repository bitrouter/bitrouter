pub mod admin;
pub mod agents;
pub mod agentskills;
pub mod anthropic;
pub(crate) mod context;
pub mod google;
pub mod mcp;
pub mod models;
pub mod openai;
pub mod routes;
pub mod tools;

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

pub(crate) use observe_ctx::StreamObserveContext;
