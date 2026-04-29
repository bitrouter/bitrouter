//! Options for calling a language model, including prompt, generation parameters, tools, and provider-specific options

use http::HeaderMap;
use tokio_util::sync::CancellationToken;

use crate::models::shared::{provider::ProviderOptions, types::JsonSchema};
use crate::observe::TraceContext;

use super::{
    prompt::LanguageModelPrompt, tool::LanguageModelTool, tool_choice::LanguageModelToolChoice,
};

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

    /// Provider-specific options that can be used to pass additional information to the provider or control provider-specific behavior
    pub provider_options: Option<ProviderOptions>,

    /// Distributed trace / session attribution for observability exporters.
    ///
    /// When present, the OTLP exporter constructs spans with the conversation
    /// and user identifiers attached. When absent, exporters generate a fresh
    /// trace ID and emit unparented spans. Populated by the API handler from
    /// `X-Bitrouter-Session-Id` / `X-Bitrouter-User-Id` headers and the
    /// OpenRouter-compatible `session_id` body field.
    pub trace_context: Option<TraceContext>,
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
