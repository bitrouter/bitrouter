use std::fmt;

use serde::{Deserialize, Serialize};

use crate::errors::Result;

// ── API protocol ──────────────────────────────────────────────────

/// The API protocol / wire format that an endpoint uses.
///
/// Model protocols determine how LLM requests are serialized (OpenAI chat
/// completions, Anthropic messages, Google generative AI). Tool protocols
/// determine how tool discovery and invocation work (MCP, A2A, REST, Skill).
///
/// A provider may default to one protocol but individual endpoints can
/// override it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    // Model protocols
    Openai,
    Anthropic,
    Google,
    // Tool protocols
    Mcp,
    A2a,
    Rest,
    Skill,
}

impl fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Mcp => "mcp",
            Self::A2a => "a2a",
            Self::Rest => "rest",
            Self::Skill => "skill",
        })
    }
}

// ── Model routing ─────────────────────────────────────────────────

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
    /// The API protocol the provider uses.
    pub protocol: ApiProtocol,
}

/// Input token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InputTokenPricing {
    /// Cost per million non-cached input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_cache: Option<f64>,
    /// Cost per million cache-read input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cost per million cache-write input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl InputTokenPricing {
    fn is_empty(&self) -> bool {
        self.no_cache.is_none() && self.cache_read.is_none() && self.cache_write.is_none()
    }
}

/// Output token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OutputTokenPricing {
    /// Cost per million text output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<f64>,
    /// Cost per million reasoning output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
}

impl OutputTokenPricing {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.reasoning.is_none()
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
    /// Returns `true` when no pricing data is set.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_empty() && self.output_tokens.is_empty()
    }
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
