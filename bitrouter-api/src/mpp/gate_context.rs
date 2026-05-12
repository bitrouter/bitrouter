//! Shared per-request context for `_with_payment_gate` / `_with_mpp` handlers.

use std::sync::Arc;

use bitrouter_core::observe::{CallerContext, MetadataHook, ObserveCallback};
use bitrouter_core::routers::router::DynChainOverlay;

use crate::fallback::FallbackPolicy;

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
    /// Optional per-request hook that mutates the resolved routing chain
    /// after `RoutingTable::route_chain()` but before
    /// `LanguageModelRouter::route_model()`. `None` is the no-op default.
    pub chain_overlay: Option<Arc<DynChainOverlay<'static>>>,
    /// Policy that decides whether a per-target failure advances the
    /// chain or surfaces immediately. Filters wrap a missing value with
    /// [`crate::fallback::default_fallback_policy`].
    pub fallback_policy: Arc<dyn FallbackPolicy>,
}

/// Filter-construction options that tune how the payment-gate handler
/// builds and iterates its routing chain. Bundled into a single struct
/// so the public `*_with_payment_gate` constructors stay under clippy's
/// `too_many_arguments` threshold; both fields are independently
/// defaultable, so the no-op case is just
/// [`PaymentGateOverlayOptions::default()`].
#[derive(Default, Clone)]
pub struct PaymentGateOverlayOptions {
    /// Per-request hook applied to the routing chain between
    /// `RoutingTable::route_chain()` and the model invocation. Anonymous
    /// routers inject candidate providers here; BYOK consumers inject
    /// per-target credentials. `None` skips the overlay step.
    pub chain_overlay: Option<Arc<DynChainOverlay<'static>>>,
    /// Policy deciding whether a per-target failure advances the chain
    /// or surfaces immediately. `None` falls back to
    /// [`crate::fallback::default_fallback_policy`] (4xx → stop, 5xx and
    /// transport → fallback), which is appropriate for direct-routing
    /// callers; anonymous-router consumers will typically pass a custom
    /// policy that treats any provider-tagged error as `Fallback`.
    pub fallback_policy: Option<Arc<dyn FallbackPolicy>>,
}
