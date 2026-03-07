use regex::Regex;

use crate::models::shared::types::Record;

use super::{
    call_options::LanguageModelCallOptions, generate_result::LanguageModelGenerateResult,
    stream_result::LanguageModelStreamResult,
};

/// The main trait for a language model, which can generate content based on a prompt and options
pub trait LanguageModel {
    /// Provider name, e.g. "openai", "anthropic", etc.
    fn provider_name(&self) -> &str;

    /// Model ID
    fn model_id(&self) -> &str;

    /// Media type -> Regex for supported URLs of that media type
    ///
    /// Matched URLs are supported natively by the model and are not downloaded.
    fn supported_urls(&self) -> impl Future<Output = Record<String, Regex>>;

    /// Generates content based on the given options.
    fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = LanguageModelGenerateResult>;

    /// Generates content based on the given options, but returns a stream of partial results.
    fn stream(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = LanguageModelStreamResult>;
}
