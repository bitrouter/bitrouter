use std::sync::Arc;

use bitrouter_core::{
    errors::Result,
    models::language::{
        language_model::{DynLanguageModel, LanguageModel},
        stream_result::LanguageModelStreamResult,
    },
    routers::{router::LanguageModelRouter, routing_table::RoutingTarget},
};

use crate::engine::Guardrail;
use crate::guarded_model::GuardedModel;

/// A [`LanguageModelRouter`] wrapper that applies guardrail inspection to
/// every model returned by the inner router.
///
/// When the guardrail is disabled (`enabled: false` in config) the wrapper
/// is a zero-cost pass-through — it returns the inner model unchanged.
pub struct GuardedRouter<R> {
    inner: R,
    guardrail: Arc<Guardrail>,
}

impl<R> GuardedRouter<R> {
    /// Wrap an existing router with guardrail enforcement.
    pub fn new(inner: R, guardrail: Arc<Guardrail>) -> Self {
        Self { inner, guardrail }
    }
}

impl<R> LanguageModelRouter for GuardedRouter<R>
where
    R: std::ops::Deref + Send + Sync,
    R::Target: LanguageModelRouter + Send + Sync,
{
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        let model = self.inner.route_model(target).await?;

        if self.guardrail.is_disabled() {
            return Ok(model);
        }

        Ok(DynLanguageModel::new_box(GuardedModel::new(
            model,
            self.guardrail.clone(),
        )))
    }
}

/// A [`LanguageModel`] wrapper that runs guardrail inspection on every
/// `generate` and `stream` call.
///
/// - **Upgoing**: call options are inspected (and optionally redacted or
///   blocked) before being forwarded to the inner model.
/// - **Downgoing**: generate results and individual stream parts are
///   inspected after the inner model produces them.
impl LanguageModel for GuardedModel {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    async fn supported_urls(
        &self,
    ) -> bitrouter_core::models::shared::types::Record<String, regex::Regex> {
        self.inner.supported_urls().await
    }

    async fn generate(
        &self,
        mut options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
    ) -> Result<bitrouter_core::models::language::generate_result::LanguageModelGenerateResult>
    {
        // Upgoing inspection
        self.guardrail
            .inspect_call_options(&mut options)
            .map_err(|reason| {
                bitrouter_core::errors::BitrouterError::invalid_request(
                    Some(self.inner.provider_name()),
                    reason,
                    None,
                )
            })?;

        // Forward to inner model
        let mut result = self.inner.generate(options).await?;

        // Downgoing inspection
        self.guardrail
            .inspect_generate_result(&mut result)
            .map_err(|reason| {
                bitrouter_core::errors::BitrouterError::invalid_response(
                    Some(self.inner.provider_name()),
                    reason,
                    None,
                )
            })?;

        Ok(result)
    }

    async fn stream(
        &self,
        mut options: bitrouter_core::models::language::call_options::LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        // Upgoing inspection
        self.guardrail
            .inspect_call_options(&mut options)
            .map_err(|reason| {
                bitrouter_core::errors::BitrouterError::invalid_request(
                    Some(self.inner.provider_name()),
                    reason,
                    None,
                )
            })?;

        // Forward to inner model
        let result = self.inner.stream(options).await?;

        // Wrap the stream with downgoing inspection
        let guarded_stream = GuardedStream::new(result.stream, self.guardrail.clone());

        Ok(LanguageModelStreamResult {
            stream: Box::pin(guarded_stream),
            request: result.request,
            response: result.response,
        })
    }
}

// ── Guarded stream adapter ──────────────────────────────────────────────

use std::pin::Pin;
use std::task::{Context, Poll};

use bitrouter_core::models::language::stream_part::LanguageModelStreamPart;

/// A stream adapter that inspects each [`LanguageModelStreamPart`] through
/// the guardrail engine. Parts that trigger `Block` are converted to
/// [`LanguageModelStreamPart::Error`]; parts that trigger `Redact` are
/// mutated in place.
struct GuardedStream {
    inner: Pin<Box<dyn futures_core::Stream<Item = LanguageModelStreamPart> + Send>>,
    guardrail: Arc<Guardrail>,
}

impl GuardedStream {
    fn new(
        inner: Pin<Box<dyn futures_core::Stream<Item = LanguageModelStreamPart> + Send>>,
        guardrail: Arc<Guardrail>,
    ) -> Self {
        Self { inner, guardrail }
    }
}

impl futures_core::Stream for GuardedStream {
    type Item = LanguageModelStreamPart;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(mut part)) => match self.guardrail.inspect_stream_part(&mut part) {
                Ok(_) => Poll::Ready(Some(part)),
                Err(reason) => {
                    tracing::warn!(%reason, "guardrail blocked stream part");
                    Poll::Ready(Some(LanguageModelStreamPart::Error {
                        error: serde_json::json!({
                            "error": {
                                "message": reason,
                                "type": "guardrail_blocked",
                            }
                        }),
                    }))
                }
            },
        }
    }
}
