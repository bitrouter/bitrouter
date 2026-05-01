mod model;
mod router;
mod stream;

pub use model::HookedModel;
pub use router::HookedRouter;

use crate::errors::BitrouterError;
use crate::models::language::{
    generate_result::LanguageModelGenerateResult, stream_part::LanguageModelStreamPart,
};

/// Identity of the model that handled a generation.
///
/// Passed to every [`GenerationHook`] callback so hooks can attribute
/// results without needing access to the original request.
#[derive(Debug, Clone)]
pub struct GenerationContext<'a> {
    /// Upstream provider model ID (e.g. `"meta-llama/Llama-4-Maverick-17B-128E-Instruct"`).
    pub model_id: &'a str,
    /// Provider name (e.g. `"chutes-ai"`).
    pub provider_name: &'a str,
}

/// A hook that observes generation lifecycle events for side-effect purposes
/// (logging, metrics, token tracking, auditing).
///
/// Hooks receive borrowed references to core types and must not block.
/// All methods have default no-op implementations so consumers only
/// override the events they care about.
pub trait GenerationHook: Send + Sync {
    /// Called after a non-streaming `generate()` call completes successfully.
    fn on_generate_result(
        &self,
        _ctx: &GenerationContext<'_>,
        _result: &LanguageModelGenerateResult,
    ) {
    }

    /// Called when `generate()` or `stream()` returns an error.
    fn on_generate_error(&self, _error: &BitrouterError) {}

    /// Called for each streaming part as it is yielded from the model stream.
    ///
    /// To capture token usage from streaming responses, match on
    /// [`LanguageModelStreamPart::Finish`] which carries
    /// [`LanguageModelUsage`](crate::models::language::usage::LanguageModelUsage).
    fn on_stream_part(&self, _ctx: &GenerationContext<'_>, _part: &LanguageModelStreamPart) {}
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, atomic::AtomicU32};

    use crate::models::language::{
        finish_reason::LanguageModelFinishReason,
        generate_result::{LanguageModelGenerateResult, LanguageModelRawRequest},
        stream_part::LanguageModelStreamPart,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    };

    use super::stream::HookedStream;
    use super::*;

    fn test_usage() -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(10),
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(20),
                text: None,
                reasoning: None,
            },
            raw: None,
        }
    }

    fn test_generate_result() -> LanguageModelGenerateResult {
        LanguageModelGenerateResult {
            content: vec![
                crate::models::language::content::LanguageModelContent::Text {
                    text: String::new(),
                    provider_metadata: None,
                },
            ],
            finish_reason: LanguageModelFinishReason::Stop,
            usage: test_usage(),
            provider_metadata: None,
            request: Some(LanguageModelRawRequest {
                headers: None,
                body: serde_json::json!({}),
            }),
            response_metadata: None,
            warnings: None,
        }
    }

    /// A test hook that counts invocations.
    struct CountingHook {
        generate_count: AtomicU32,
        error_count: AtomicU32,
        stream_count: AtomicU32,
    }

    impl CountingHook {
        fn new() -> Self {
            Self {
                generate_count: AtomicU32::new(0),
                error_count: AtomicU32::new(0),
                stream_count: AtomicU32::new(0),
            }
        }
    }

    impl GenerationHook for CountingHook {
        fn on_generate_result(
            &self,
            _ctx: &GenerationContext<'_>,
            _result: &LanguageModelGenerateResult,
        ) {
            self.generate_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn on_generate_error(&self, _error: &crate::errors::BitrouterError) {
            self.error_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn on_stream_part(&self, _ctx: &GenerationContext<'_>, _part: &LanguageModelStreamPart) {
            self.stream_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn default_hook_methods_are_noop() {
        struct NoopHook;
        impl GenerationHook for NoopHook {}

        let hook = NoopHook;
        let ctx = GenerationContext {
            model_id: "test-model",
            provider_name: "test-provider",
        };
        hook.on_generate_result(&ctx, &test_generate_result());
        hook.on_generate_error(&crate::errors::BitrouterError::transport(None, "test"));
        hook.on_stream_part(
            &ctx,
            &LanguageModelStreamPart::TextDelta {
                id: "t1".into(),
                delta: "hello".into(),
                provider_metadata: None,
            },
        );
    }

    #[tokio::test]
    async fn hooked_stream_invokes_hooks_for_each_part() {
        let hook = Arc::new(CountingHook::new());
        let hooks: Arc<[Arc<dyn GenerationHook>]> =
            Arc::from(vec![hook.clone() as Arc<dyn GenerationHook>]);

        let parts = vec![
            LanguageModelStreamPart::StreamStart {
                warnings: Vec::new(),
            },
            LanguageModelStreamPart::TextDelta {
                id: "t1".into(),
                delta: "hello".into(),
                provider_metadata: None,
            },
            LanguageModelStreamPart::TextDelta {
                id: "t1".into(),
                delta: " world".into(),
                provider_metadata: None,
            },
            LanguageModelStreamPart::Finish {
                usage: test_usage(),
                finish_reason: LanguageModelFinishReason::Stop,
                provider_metadata: None,
            },
        ];

        let inner: Pin<Box<dyn futures_core::Stream<Item = LanguageModelStreamPart> + Send>> =
            Box::pin(tokio_stream::iter(parts));

        let hooked = HookedStream::new(
            inner,
            hooks,
            "test-model".to_owned(),
            "test-provider".to_owned(),
        );
        let mut hooked = Box::pin(hooked);

        use tokio_stream::StreamExt as _;
        let mut collected = Vec::new();
        while let Some(part) = hooked.next().await {
            collected.push(part);
        }

        assert_eq!(collected.len(), 4);
        assert_eq!(
            hook.stream_count.load(std::sync::atomic::Ordering::SeqCst),
            4
        );
    }

    #[tokio::test]
    async fn multiple_hooks_all_invoked() {
        let hook_a = Arc::new(CountingHook::new());
        let hook_b = Arc::new(CountingHook::new());
        let hooks: Arc<[Arc<dyn GenerationHook>]> = Arc::from(vec![
            hook_a.clone() as Arc<dyn GenerationHook>,
            hook_b.clone() as Arc<dyn GenerationHook>,
        ]);

        let parts = vec![
            LanguageModelStreamPart::TextDelta {
                id: "t1".into(),
                delta: "hi".into(),
                provider_metadata: None,
            },
            LanguageModelStreamPart::Finish {
                usage: test_usage(),
                finish_reason: LanguageModelFinishReason::Stop,
                provider_metadata: None,
            },
        ];

        let inner: Pin<Box<dyn futures_core::Stream<Item = LanguageModelStreamPart> + Send>> =
            Box::pin(tokio_stream::iter(parts));

        let hooked = HookedStream::new(
            inner,
            hooks,
            "test-model".to_owned(),
            "test-provider".to_owned(),
        );
        let mut hooked = Box::pin(hooked);

        use tokio_stream::StreamExt as _;
        while hooked.next().await.is_some() {}

        assert_eq!(
            hook_a
                .stream_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            hook_b
                .stream_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn on_generate_result_invoked() {
        let hook = Arc::new(CountingHook::new());
        let result = test_generate_result();
        let ctx = GenerationContext {
            model_id: "test-model",
            provider_name: "test-provider",
        };

        hook.on_generate_result(&ctx, &result);
        hook.on_generate_result(&ctx, &result);

        assert_eq!(
            hook.generate_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[test]
    fn on_generate_error_invoked() {
        let hook = Arc::new(CountingHook::new());
        let error = crate::errors::BitrouterError::transport(None, "connection failed");

        hook.on_generate_error(&error);
        hook.on_generate_error(&error);

        assert_eq!(
            hook.error_count.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }
}
