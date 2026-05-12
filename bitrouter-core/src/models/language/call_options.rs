//! Options for calling a language model, including prompt, generation parameters, tools, and provider-specific options

use http::HeaderMap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::models::shared::{provider::ProviderOptions, types::JsonSchema};

use super::{
    prompt::LanguageModelPrompt, tool::LanguageModelTool, tool_choice::LanguageModelToolChoice,
};

/// Normalized reasoning effort across providers.
///
/// Providers expose reasoning configuration in fundamentally different shapes:
/// OpenAI uses an enum (`minimal`/`low`/`medium`/`high`), Anthropic uses an
/// explicit `budget_tokens` integer, and Google uses a `thinkingBudget` (or
/// `thinkingLevel` on Gemini 3). BitRouter normalizes through this enum and
/// lets each provider adapter map it back to the native shape. Mappings are
/// intentionally lossy at the bucket boundaries — see the per-protocol
/// `preset.rs` and provider `build_*_request` for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// Parses a string value, returning `None` for unknown variants.
    ///
    /// Comparison is ASCII case-insensitive. Used by inbound parsers to lift
    /// per-protocol string fields (OpenAI `reasoning_effort`, Google
    /// `thinkingLevel`) into the normalized enum.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// String form accepted by OpenAI's `reasoning_effort` field
    /// (Chat Completions) and `reasoning.effort` (Responses).
    pub fn as_openai_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Token budget for Anthropic's `thinking.budget_tokens` field. `None`
    /// means "disable thinking" (mapped to `{type: "disabled"}` upstream).
    ///
    /// Returned values respect Anthropic's documented minimum of 1024. The
    /// outbound provider must additionally clamp the budget to be strictly
    /// less than the request's `max_tokens`.
    pub fn anthropic_budget_tokens(self) -> Option<u32> {
        match self {
            Self::Minimal => None,
            Self::Low => Some(1024),
            Self::Medium => Some(4096),
            Self::High => Some(16384),
        }
    }

    /// Token budget for Google's `thinkingConfig.thinkingBudget` field
    /// (Gemini 2.5). `0` disables thinking on Flash models; Pro models
    /// require a minimum of 128 and will reject 0.
    pub fn google_thinking_budget(self) -> i32 {
        match self {
            Self::Minimal => 0,
            Self::Low => 1024,
            Self::Medium => 4096,
            Self::High => 16384,
        }
    }
}

/// Options for calling a language model
#[derive(Debug, Clone)]
pub struct LanguageModelCallOptions {
    /// The prompt to send to the language model, which is a sequence of messages from the system, user, assistant, and tools
    pub prompt: LanguageModelPrompt,
    /// Whether to stream the response as it's generated, or return it all at once when complete
    pub stream: Option<bool>,
    /// The maximum number of tokens to generate in the response
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature to use, between 0 and 1
    pub temperature: Option<f32>,
    /// Top-p (nucleus) sampling probability
    pub top_p: Option<f32>,
    /// Top-k sampling
    pub top_k: Option<u32>,
    /// Stop sequences to end generation when encountered
    pub stop_sequences: Option<Vec<String>>,
    /// Presence penalty to penalize new tokens based on whether they appear in the prompt
    pub presence_penalty: Option<f32>,
    /// Frequency penalty to penalize new tokens based on their frequency in the prompt
    pub frequency_penalty: Option<f32>,
    /// The format of the response
    pub response_format: Option<LanguageModelResponseFormat>,
    /// Seed for random number generation
    pub seed: Option<u64>,
    /// Tools available to the language model
    pub tools: Option<Vec<LanguageModelTool>>,
    /// The tool choice strategy to use when the model calls a tool
    pub tool_choice: Option<LanguageModelToolChoice>,
    /// Whether to include raw chunks in the response
    pub include_raw_chunks: Option<bool>,
    /// Signal to abort the request
    pub abort_signal: Option<CancellationToken>,
    /// Custom headers to include in the request
    pub headers: Option<HeaderMap>,

    /// Normalized reasoning effort. Each provider adapter maps this to the
    /// native shape (OpenAI `reasoning_effort` string, Anthropic `thinking`
    /// object, Google `thinkingConfig`).
    pub reasoning_effort: Option<ReasoningEffort>,

    /// Provider-specific options that can be used to pass additional information to the provider or control provider-specific behavior
    pub provider_options: Option<ProviderOptions>,
}

#[derive(Debug, Clone)]
pub enum LanguageModelResponseFormat {
    /// The response should be returned as text
    Text,
    /// Structured JSON response, with optional schema for validation
    Json {
        /// Optional JSON schema to validate the output against
        schema: Option<JsonSchema>,
        /// The name of the output
        name: Option<String>,
        /// Description of the object that should be generated
        description: Option<String>,
    },
}
