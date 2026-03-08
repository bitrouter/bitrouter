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
pub trait LanguageModel {
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

// ── Send-safe boxed wrapper ─────────────────────────────────────────────────

use std::pin::Pin;

/// Object-safe helper trait with Send + Sync bounds for dynamic dispatch.
trait ErasedLanguageModel: Send + Sync {
    fn provider_name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn supported_urls_boxed(
        &self,
    ) -> Pin<Box<dyn Future<Output = Record<String, Regex>> + Send + '_>>;
    fn generate_boxed(
        &self,
        options: LanguageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LanguageModelGenerateResult>> + Send + '_>>;
    fn stream_boxed(
        &self,
        options: LanguageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LanguageModelStreamResult>> + Send + '_>>;
}

impl<T: LanguageModel + Send + Sync> ErasedLanguageModel for T {
    fn provider_name(&self) -> &str {
        LanguageModel::provider_name(self)
    }
    fn model_id(&self) -> &str {
        LanguageModel::model_id(self)
    }
    fn supported_urls_boxed(
        &self,
    ) -> Pin<Box<dyn Future<Output = Record<String, Regex>> + Send + '_>> {
        Box::pin(self.supported_urls())
    }
    fn generate_boxed(
        &self,
        options: LanguageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LanguageModelGenerateResult>> + Send + '_>> {
        Box::pin(self.generate(options))
    }
    fn stream_boxed(
        &self,
        options: LanguageModelCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LanguageModelStreamResult>> + Send + '_>> {
        Box::pin(self.stream(options))
    }
}

/// A boxed, Send + Sync wrapper around any [`LanguageModel`] implementation.
///
/// This enables dynamic dispatch across different model types while satisfying
/// the Send + Sync bounds required by async runtimes (tokio, warp, etc.).
///
/// Use [`BoxLanguageModel::new`] to create an instance from any concrete model type.
pub struct BoxLanguageModel {
    inner: Box<dyn ErasedLanguageModel>,
}

// SAFETY: BoxLanguageModel wraps a `dyn ErasedLanguageModel` which requires Send + Sync.
unsafe impl Send for BoxLanguageModel {}
unsafe impl Sync for BoxLanguageModel {}

impl BoxLanguageModel {
    /// Creates a new `BoxLanguageModel` from any concrete model that implements
    /// [`LanguageModel`] + `Send` + `Sync`.
    pub fn new<T: LanguageModel + Send + Sync + 'static>(model: T) -> Self {
        Self {
            inner: Box::new(model),
        }
    }
}

impl LanguageModel for BoxLanguageModel {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn supported_urls(&self) -> impl Future<Output = Record<String, Regex>> + Send {
        self.inner.supported_urls_boxed()
    }

    fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelGenerateResult>> + Send {
        self.inner.generate_boxed(options)
    }

    fn stream(
        &self,
        options: LanguageModelCallOptions,
    ) -> impl Future<Output = Result<LanguageModelStreamResult>> + Send {
        self.inner.stream_boxed(options)
    }
}
