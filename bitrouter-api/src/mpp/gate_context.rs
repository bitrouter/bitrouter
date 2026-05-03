//! Shared per-request context for `_with_payment_gate` / `_with_mpp` handlers.

use std::sync::Arc;

use bitrouter_core::observe::{CallerContext, MetadataHook, ObserveCallback};
use bitrouter_core::routers::router::DynTargetOverlay;

use super::PaymentGate;

/// Bundled request-scoped state passed into `handle_*_with_gate` helpers.
///
/// Bundling these here keeps the per-handler signatures under
/// clippy's `too_many_arguments` threshold while still making the
/// individual fields directly accessible inside handlers.
pub struct GateContext {
    pub caller: CallerContext,
    pub payment_gate: Arc<dyn PaymentGate>,
    pub auth_header: Option<String>,
    pub observer: Arc<dyn ObserveCallback>,
    pub metadata_hook: MetadataHook,
    pub origin: Option<String>,
    /// Optional per-request hook that mutates the routing target after
    /// resolution. Invoked between `RoutingTable::route()` and
    /// `LanguageModelRouter::route_model()`. `None` is the no-op default.
    pub target_overlay: Option<Arc<DynTargetOverlay<'static>>>,
}
