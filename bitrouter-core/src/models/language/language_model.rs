use dynosaur::dynosaur;
use regex::Regex;

use crate::{errors::Result, models::shared::types::Record};

use super::{
    call_options::LanguageModelCallOptions, generate_result::LanguageModelGenerateResult,
    stream_result::LanguageModelStreamResult,
};

/// The main trait for a language model provider, which can generate content based on a prompt and options.
///
/// Each implementation represents a concrete upstream model instance (e.g. "gpt-4o via OpenAI chat completions",
/// "claude-3-5-sonnet via Anthropic messages"). The model ID is stored on the instance,
/// not passed per-request.
#[dynosaur(pub DynLanguageModel = dyn(box) LanguageModel)]
pub trait LanguageModel: Send + Sync {
    /// Provider name, e.g. "openai", "anthropic", etc.
    fn provider_name(&self) -> &str;

    /// The upstream model ID, e.g. "gpt-4o", "claude-3-5-sonnet-20241022", etc.
    fn model_id(&self) -> &str;

    /// Media type -> Regex for supported URLs of that media type
    ///
    /// Matched URLs are supported natively by the model and are not downloaded.
    fn supported_urls(&self) -> impl Future<Output = Record<String, Regex>> + Send;

    /// Generates content based on the given options.
    fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelGenerateResult>> + Send;

    /// Generates content based on the given options, but returns a stream of partial results.
    fn stream(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelStreamResult>> + Send;
}
