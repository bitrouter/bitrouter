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

/// A single model available through a provider, with its metadata.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// The upstream model ID (e.g. "gpt-4o", "claude-sonnet-4-20250514").
    pub id: String,
    /// The provider that offers this model.
    pub provider: String,
    /// Human-readable display name.
    pub name: Option<String>,
    /// Brief description of the model's capabilities.
    pub description: Option<String>,
    /// Maximum input context window in tokens.
    pub max_input_tokens: Option<u64>,
    /// Maximum number of output tokens the model can produce.
    pub max_output_tokens: Option<u64>,
    /// Input modalities the model accepts (e.g. "text", "image").
    pub input_modalities: Vec<String>,
    /// Output modalities the model can produce.
    pub output_modalities: Vec<String>,
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

    /// Lists all models available across all configured providers.
    fn list_models(&self) -> Vec<ModelEntry> {
        Vec::new()
    }
}
