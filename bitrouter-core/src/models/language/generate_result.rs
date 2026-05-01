use http::HeaderMap;

use crate::models::shared::{
    provider::ProviderMetadata, types::TimestampMillis, warnings::Warning,
};

use super::{
    content::LanguageModelContent, finish_reason::LanguageModelFinishReason,
    usage::LanguageModelUsage,
};

/// Represents the result of a Language Model generation.
#[derive(Debug, Clone)]
pub struct LanguageModelGenerateResult {
    /// The generated content blocks, in the order returned by the provider.
    ///
    /// A single assistant turn may contain multiple ordered blocks, e.g.
    /// explanatory text followed by one or more `tool-call` blocks. This is
    /// required for compatibility with Anthropic Messages-style responses and
    /// OpenAI-compatible chat completions that include both `message.content`
    /// and `message.tool_calls` in the same choice.
    pub content: Vec<LanguageModelContent>,
    /// The finish reason, if the generation is complete
    pub finish_reason: LanguageModelFinishReason,
    /// The usage information for this generation
    pub usage: LanguageModelUsage,
    /// Provider-specific metadata for this generation result
    pub provider_metadata: Option<ProviderMetadata>,
    /// The original request that led to this generation result, if available
    pub request: Option<LanguageModelRawRequest>,
    /// The original response from the provider, if available
    pub response_metadata: Option<LanguageModelRawResponse>,
    /// Any warnings related to this generation result
    pub warnings: Option<Vec<Warning>>,
}

/// Represents the raw request sent to a Language Model provider.
#[derive(Debug, Clone)]
pub struct LanguageModelRawRequest {
    /// The request headers as a map of header name to value
    pub headers: Option<HeaderMap>,
    /// The request body as JSON value
    pub body: serde_json::Value,
}

/// Represents the raw response received from a Language Model provider.
#[derive(Debug, Clone)]
pub struct LanguageModelRawResponse {
    /// The unique identifier for this response
    pub id: Option<String>,
    /// The timestamp when the response was received, in milliseconds since the Unix epoch
    pub timestamp: Option<TimestampMillis>,
    /// The model identifier used for this response
    pub model_id: Option<String>,
    /// The response headers as a map of header name to value
    pub headers: Option<HeaderMap>,
    /// The response body as JSON value
    pub body: Option<serde_json::Value>,
}
