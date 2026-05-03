use dynosaur::dynosaur;

use crate::{
    agents::provider::DynAgentProvider,
    errors::Result,
    models::{image::image_model::DynImageModel, language::language_model::DynLanguageModel},
    observe::CallerContext,
    routers::routing_table::RoutingTarget,
    tools::provider::DynToolProvider,
};

/// A router that routes to the appropriate language model implementation based on the routing target.
pub trait LanguageModelRouter {
    /// Routes to the appropriate language model implementation based on the routing target.
    ///
    /// Returns a dynosaur-generated [`DynLanguageModel`] that can hold any
    /// concrete model type while keeping the dynamic-dispatch boilerplate in one place.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynLanguageModel<'static>>>> + Send;
}

/// A router that routes to the appropriate image model implementation based on the routing target.
pub trait ImageModelRouter {
    /// Routes to the appropriate image model implementation based on the routing target.
    fn route_model(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynImageModel<'static>>>> + Send;
}

/// A router that resolves an agent name to the appropriate agent
/// provider implementation.
///
/// Unlike model and tool routers (which take a [`RoutingTarget`]),
/// agent routers take the agent name directly. Agents are long-lived
/// named processes, not fungible routed services.
pub trait AgentRouter {
    /// Resolves the agent name to a concrete agent provider.
    fn route_agent(
        &self,
        agent_name: &str,
    ) -> impl Future<Output = Result<Box<DynAgentProvider<'static>>>> + Send;
}

/// A router that resolves a routing target to the appropriate tool
/// provider implementation.
///
/// This is the tool equivalent of [`LanguageModelRouter`]. Unlike the model
/// router (which instantiates providers per-request), tool providers are
/// typically long-lived connections constructed at startup, so
/// implementations will look up an existing provider rather than creating
/// a new one.
pub trait ToolRouter {
    /// Resolves the routing target to a concrete tool provider.
    fn route_tool(
        &self,
        target: RoutingTarget,
    ) -> impl Future<Output = Result<Box<DynToolProvider<'static>>>> + Send;
}

/// A hook that mutates a [`RoutingTarget`] after routing but before model
/// instantiation, given the request's [`CallerContext`].
///
/// Used to inject per-caller credentials or base URLs into the target —
/// for example, a "bring your own key" (BYOK) overlay that looks up the
/// caller's stored API key for the resolved provider and sets
/// [`RoutingTarget::api_key_override`].
///
/// Implementations should be cheap on the no-op path (e.g. anonymous caller,
/// or caller with no overlay configured), since the hook runs on every
/// request that the host filter receives.
#[dynosaur(pub DynTargetOverlay = dyn(box) TargetOverlay)]
pub trait TargetOverlay: Send + Sync {
    /// Inspects `caller` and optionally mutates `target`.
    ///
    /// Returning `Err` aborts the request. Implementations should choose the
    /// returned [`BitrouterError`](crate::errors::BitrouterError) variant
    /// deliberately, because the host filter surfaces it to the client:
    ///
    /// - **Stored credential is invalid/expired/revoked**: return
    ///   [`BitrouterError::AccessDenied`](crate::errors::BitrouterError::AccessDenied)
    ///   — surfaces as `403`.
    /// - **Decryption / parse failure on a stored credential**: return
    ///   [`BitrouterError::InvalidRequest`](crate::errors::BitrouterError::InvalidRequest)
    ///   — surfaces as `400` and signals "fix your stored credential."
    /// - Reserve errors for genuine failures and return `Ok(())` for the
    ///   common no-op case.
    fn apply(
        &self,
        target: &mut RoutingTarget,
        caller: &CallerContext,
    ) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::routers::routing_table::ApiProtocol;

    struct StaticKeyOverlay {
        key: String,
    }

    impl TargetOverlay for StaticKeyOverlay {
        async fn apply(&self, target: &mut RoutingTarget, _caller: &CallerContext) -> Result<()> {
            target.api_key_override = Some(self.key.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn dyn_overlay_mutates_target_through_arc() {
        // Verifies the dynosaur-generated wrapper round-trips through Arc and
        // applies the overlay's mutation to the target. This is the same
        // shape filters use to invoke an overlay.
        let overlay: Arc<DynTargetOverlay<'static>> =
            Arc::from(DynTargetOverlay::new_box(StaticKeyOverlay {
                key: "sk-byok-from-overlay".to_owned(),
            }));
        let mut target = RoutingTarget {
            provider_name: "openai".to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_protocol: ApiProtocol::Openai,
            api_key_override: None,
            api_base_override: None,
        };
        let caller = CallerContext::default();

        overlay.apply(&mut target, &caller).await.unwrap();

        assert_eq!(
            target.api_key_override.as_deref(),
            Some("sk-byok-from-overlay")
        );
    }
}
