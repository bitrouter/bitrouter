use crate::errors::Result;

/// The target to route a request to.
pub struct RoutingTarget {
    /// The provider name to route to.
    pub provider_name: String,
    /// The actual upstream provider's model ID to route to.
    pub model_id: String,
}

/// A single entry in the route listing, describing a configured model route.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// The virtual model name (e.g. "default", "my-gpt4").
    pub model: String,
    /// The provider name this model routes to.
    pub provider: String,
    /// The API protocol the provider uses ("openai", "anthropic", "google").
    pub protocol: String,
}

/// A routing table that maps incoming model names to routing targets (provider + model ID).
pub trait RoutingTable {
    /// Routes an incoming model name to a routing target.
    fn route(
        &self,
        incoming_model_name: &str,
    ) -> impl Future<Output = Result<RoutingTarget>> + Send;

    /// Lists all configured model routes.
    fn list_routes(&self) -> Vec<RouteEntry> {
        Vec::new()
    }
}
