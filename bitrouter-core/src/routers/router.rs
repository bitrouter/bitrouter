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

/// A hook that mutates a routing chain after routing but before model
/// instantiation, given the request's [`CallerContext`].
///
/// Implementations can inject per-caller credentials into one or more targets
/// or append/remove fallback targets before the API handler starts attempting
/// the chain.
#[dynosaur(pub DynChainOverlay = dyn(box) ChainOverlay)]
pub trait ChainOverlay: Send + Sync {
    /// Inspects `caller` and optionally mutates `chain`.
    fn apply(
        &self,
        chain: &mut Vec<RoutingTarget>,
        caller: &CallerContext,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Adapter that applies an existing [`TargetOverlay`] to the first target in a
/// routing chain.
pub struct SingleTarget<T> {
    overlay: T,
}

impl<T> SingleTarget<T> {
    pub fn new(overlay: T) -> Self {
        Self { overlay }
    }
}

impl<T> ChainOverlay for SingleTarget<T>
where
    T: TargetOverlay,
{
    async fn apply(&self, chain: &mut Vec<RoutingTarget>, caller: &CallerContext) -> Result<()> {
        if let Some(target) = chain.first_mut() {
            self.overlay.apply(target, caller).await?;
        }
        Ok(())
    }
}

/// Adapter that applies a [`TargetOverlay`] to every target in a routing chain.
///
/// Unlike [`SingleTarget`] which only touches the first chain element, this
/// adapter visits every target — useful for overlays whose decision is purely
/// per-target (e.g. BYOK credential injection that needs to be checked for
/// each candidate provider in a fallback chain).
pub struct PerTarget<T> {
    overlay: T,
}

impl<T> PerTarget<T> {
    pub fn new(overlay: T) -> Self {
        Self { overlay }
    }
}

impl<T> ChainOverlay for PerTarget<T>
where
    T: TargetOverlay,
{
    async fn apply(&self, chain: &mut Vec<RoutingTarget>, caller: &CallerContext) -> Result<()> {
        for target in chain.iter_mut() {
            self.overlay.apply(target, caller).await?;
        }
        Ok(())
    }
}

/// Composes two [`ChainOverlay`]s, applying `first` then `second` to the
/// shared mutable chain.
///
/// Use this to thread a single `DynChainOverlay` through filter signatures
/// when more than one overlay needs to run on every request.
pub struct ComposedChainOverlay<A, B> {
    first: A,
    second: B,
}

impl<A, B> ComposedChainOverlay<A, B> {
    pub fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<A, B> ChainOverlay for ComposedChainOverlay<A, B>
where
    A: ChainOverlay,
    B: ChainOverlay,
{
    async fn apply(&self, chain: &mut Vec<RoutingTarget>, caller: &CallerContext) -> Result<()> {
        self.first.apply(chain, caller).await?;
        self.second.apply(chain, caller).await?;
        Ok(())
    }
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
///
/// Deprecated: use [`ChainOverlay`]. This trait is retained for one release to
/// support migration and will be removed in bitrouter-core 0.31.
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
            preset: None,
        };
        let caller = CallerContext::default();

        overlay.apply(&mut target, &caller).await.unwrap();

        assert_eq!(
            target.api_key_override.as_deref(),
            Some("sk-byok-from-overlay")
        );
    }

    struct AppendProviderOverlay {
        suffix: String,
    }

    impl ChainOverlay for AppendProviderOverlay {
        async fn apply(
            &self,
            chain: &mut Vec<RoutingTarget>,
            _caller: &CallerContext,
        ) -> Result<()> {
            for target in chain.iter_mut() {
                target.provider_name.push_str(&self.suffix);
            }
            Ok(())
        }
    }

    fn target(provider: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: provider.to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_protocol: ApiProtocol::Openai,
            api_key_override: None,
            api_base_override: None,
            preset: None,
        }
    }

    #[tokio::test]
    async fn per_target_adapter_applies_overlay_to_all_targets() {
        let overlay = PerTarget::new(StaticKeyOverlay {
            key: "sk-shared".to_owned(),
        });
        let mut chain = vec![target("openai"), target("anthropic"), target("google")];
        let caller = CallerContext::default();

        overlay.apply(&mut chain, &caller).await.unwrap();

        for target in &chain {
            assert_eq!(target.api_key_override.as_deref(), Some("sk-shared"));
        }
    }

    #[tokio::test]
    async fn composed_chain_overlay_applies_both_in_order() {
        let first = AppendProviderOverlay {
            suffix: "-1".to_owned(),
        };
        let second = AppendProviderOverlay {
            suffix: "-2".to_owned(),
        };
        let composed = ComposedChainOverlay::new(first, second);
        let mut chain = vec![target("provider")];
        let caller = CallerContext::default();

        composed.apply(&mut chain, &caller).await.unwrap();

        assert_eq!(chain[0].provider_name, "provider-1-2");
    }

    #[tokio::test]
    async fn single_target_adapter_mutates_first_chain_target() {
        let overlay = SingleTarget::new(StaticKeyOverlay {
            key: "sk-byok-from-chain-overlay".to_owned(),
        });
        let mut chain = vec![
            RoutingTarget {
                provider_name: "openai".to_owned(),
                service_id: "gpt-4o".to_owned(),
                api_protocol: ApiProtocol::Openai,
                api_key_override: None,
                api_base_override: None,
                preset: None,
            },
            RoutingTarget {
                provider_name: "anthropic".to_owned(),
                service_id: "claude-sonnet-4".to_owned(),
                api_protocol: ApiProtocol::Anthropic,
                api_key_override: None,
                api_base_override: None,
                preset: None,
            },
        ];
        let caller = CallerContext::default();

        assert!(overlay.apply(&mut chain, &caller).await.is_ok());

        assert_eq!(
            chain[0].api_key_override.as_deref(),
            Some("sk-byok-from-chain-overlay")
        );
        assert!(chain[1].api_key_override.is_none());
    }
}
