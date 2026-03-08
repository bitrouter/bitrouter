use regex::Regex;

use crate::{errors::Result, models::shared::types::Record};

use super::{
    call_options::LanguageModelCallOptions, generate_result::LanguageModelGenerateResult,
    stream_result::LanguageModelStreamResult,
};

/// The main trait for a language model provider, which can generate content based on a prompt and options.
///
/// Each implementation represents a provider + API kind (e.g. "OpenAI chat completions",
/// "Anthropic messages"). The model ID is passed per-request, allowing a single provider
/// instance to serve any model it supports.
pub trait LanguageModel {
    /// Provider name, e.g. "openai", "anthropic", etc.
    fn provider_name(&self) -> &str;

    /// Media type -> Regex for supported URLs of that media type
    ///
    /// Matched URLs are supported natively by the model and are not downloaded.
    fn supported_urls(&self) -> impl Future<Output = Record<String, Regex>>;

    /// Generates content based on the given options.
    fn generate(
        &self,
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelGenerateResult>>;

    /// Generates content based on the given options, but returns a stream of partial results.
    fn stream(
        &self,
        model_id: &str,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelStreamResult>>;
}
