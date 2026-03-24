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
    #[serde(skip_serializing_if = "is_zero")]
    pub no_cache: f64,
    /// Cost per million cache-read input tokens.
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_read: f64,
    /// Cost per million cache-write input tokens.
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_write: f64,
}

impl InputTokenPricing {
    fn is_empty(&self) -> bool {
        self.no_cache == 0.0 && self.cache_read == 0.0 && self.cache_write == 0.0
    }
}

/// Output token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutputTokenPricing {
    /// Cost per million text output tokens.
    #[serde(skip_serializing_if = "is_zero")]
    pub text: f64,
    /// Cost per million reasoning output tokens.
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning: f64,
}

impl OutputTokenPricing {
    fn is_empty(&self) -> bool {
        self.text == 0.0 && self.reasoning == 0.0
    }
}

/// Token pricing per million tokens for a model.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelPricing {
    #[serde(skip_serializing_if = "InputTokenPricing::is_empty")]
    pub input_tokens: InputTokenPricing,
    #[serde(skip_serializing_if = "OutputTokenPricing::is_empty")]
    pub output_tokens: OutputTokenPricing,
}

impl ModelPricing {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_empty() && self.output_tokens.is_empty()
    }
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
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
