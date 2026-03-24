use serde::Serialize;

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

/// Input token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputTokenPricing {
    /// Cost per million non-cached input tokens.
    pub no_cache: f64,
    /// Cost per million cache-read input tokens.
    pub cache_read: f64,
    /// Cost per million cache-write input tokens.
    pub cache_write: f64,
}

/// Output token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutputTokenPricing {
    /// Cost per million text output tokens.
    pub text: f64,
    /// Cost per million reasoning output tokens.
    pub reasoning: f64,
}

/// Token pricing per million tokens for a model.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelPricing {
    pub input_tokens: InputTokenPricing,
    pub output_tokens: OutputTokenPricing,
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
