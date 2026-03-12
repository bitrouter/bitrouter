use std::sync::Arc;

use crate::models::language::language_model::DynLanguageModel;

use super::GenerationHook;

/// A model wrapper that invokes [`GenerationHook`] callbacks after
/// `generate()` completes and for each streaming part yielded by `stream()`.
///
/// The wrapper is a pure observer — it never modifies requests or responses.
/// The [`LanguageModel`](crate::models::language::language_model::LanguageModel)
/// implementation lives below in this file so all trait methods are co-located.
pub struct HookedModel {
    pub(crate) inner: Box<DynLanguageModel<'static>>,
    pub(crate) hooks: Arc<[Arc<dyn GenerationHook>]>,
}

impl HookedModel {
    pub fn new(
        inner: Box<DynLanguageModel<'static>>,
        hooks: Arc<[Arc<dyn GenerationHook>]>,
    ) -> Self {
        Self { inner, hooks }
    }
}

impl crate::models::language::language_model::LanguageModel for HookedModel {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn supported_urls(&self) -> crate::models::shared::types::Record<String, regex::Regex> {
        self.inner.supported_urls().await
    }

    async fn generate(
        &self,
        options: crate::models::language::call_options::LanguageModelCallOptions,
    ) -> crate::errors::Result<crate::models::language::generate_result::LanguageModelGenerateResult>
    {
        let result = self.inner.generate(options).await?;

        for hook in self.hooks.iter() {
            hook.on_generate_result(&result);
        }

        Ok(result)
    }

    async fn stream(
        &self,
        options: crate::models::language::call_options::LanguageModelCallOptions,
    ) -> crate::errors::Result<crate::models::language::stream_result::LanguageModelStreamResult>
    {
        let result = self.inner.stream(options).await?;

        let hooked_stream = super::stream::HookedStream::new(result.stream, self.hooks.clone());

        Ok(
            crate::models::language::stream_result::LanguageModelStreamResult {
                stream: Box::pin(hooked_stream),
                request: result.request,
                response: result.response,
            },
        )
    }
}
