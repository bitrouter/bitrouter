use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::models::language::stream_part::LanguageModelStreamPart;

use super::{GenerationContext, GenerationHook};

/// A stream adapter that invokes [`GenerationHook::on_stream_part`] for each
/// yielded [`LanguageModelStreamPart`], then passes the part through unchanged.
///
/// This is a read-only observer — stream items are never modified.
pub(crate) struct HookedStream {
    inner: Pin<Box<dyn Stream<Item = LanguageModelStreamPart> + Send>>,
    hooks: Arc<[Arc<dyn GenerationHook>]>,
    model_id: String,
    provider_name: String,
}

impl HookedStream {
    pub(crate) fn new(
        inner: Pin<Box<dyn Stream<Item = LanguageModelStreamPart> + Send>>,
        hooks: Arc<[Arc<dyn GenerationHook>]>,
        model_id: String,
        provider_name: String,
    ) -> Self {
        Self {
            inner,
            hooks,
            model_id,
            provider_name,
        }
    }
}

impl Stream for HookedStream {
    type Item = LanguageModelStreamPart;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(part)) => {
                let ctx = GenerationContext {
                    model_id: &self.model_id,
                    provider_name: &self.provider_name,
                };
                for hook in self.hooks.iter() {
                    hook.on_stream_part(&ctx, &part);
                }
                Poll::Ready(Some(part))
            }
        }
    }
}
