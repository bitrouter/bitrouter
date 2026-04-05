use crate::{
    agents::provider::DynAgentProvider,
    errors::Result,
    models::{image::image_model::DynImageModel, language::language_model::DynLanguageModel},
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
