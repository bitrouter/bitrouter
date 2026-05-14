//! The `Executor` — the component that turns a resolved `RoutingTarget` plus a
//! `Prompt` into an upstream call. Phase 1 ships the trait plus a `MockExecutor`
//! for tests; Phase 2 adds the real protocol-specific HTTP executor.

use std::pin::Pin;
use std::sync::Mutex;

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::{BitrouterError, Result};
use crate::language_model::types::{
    ExecutionResult, GenerateResult, Prompt, RoutingTarget, StreamPart,
};

/// A boxed stream of canonical stream parts.
pub type StreamPartStream = Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>;

/// Performs the actual upstream call for one routing target.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute a non-streaming request against `target`.
    async fn execute(&self, target: &RoutingTarget, prompt: &Prompt) -> Result<ExecutionResult>;

    /// Start a streaming request against `target`.
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
    ) -> Result<StreamPartStream>;
}

/// One canned upstream response for `MockExecutor`.
pub enum MockResponse {
    /// A successful non-streaming result.
    Generate(GenerateResult),
    /// A successful streaming result (the part list, each emitted in order).
    Stream(Vec<StreamPart>),
    /// An error (drives fallback testing).
    Error(BitrouterError),
}

/// A scriptable executor for tests. Each call pops the next scripted response
/// (keyed by provider name when scripted per-provider, else from a flat queue).
pub struct MockExecutor {
    queue: Mutex<Vec<MockResponse>>,
}

impl MockExecutor {
    /// Build an executor that will serve `responses` in order.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            // reversed so `pop()` serves in declared order
            queue: Mutex::new(responses.into_iter().rev().collect()),
        }
    }

    /// Build an executor that always returns one successful text result.
    pub fn always_text(text: impl Into<String>) -> Self {
        use crate::language_model::types::{Content, FinishReason, Usage};
        let result = GenerateResult {
            content: vec![Content::Text { text: text.into() }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                reasoning_tokens: 0,
            }),
            finish_reason: Some(FinishReason::Stop),
        };
        Self::new(vec![MockResponse::Generate(result)])
    }

    fn next(&self) -> Result<MockResponse> {
        self.queue
            .lock()
            .expect("mock executor lock poisoned")
            .pop()
            .ok_or_else(|| BitrouterError::internal("MockExecutor: no scripted response left"))
    }
}

#[async_trait]
impl Executor for MockExecutor {
    async fn execute(&self, target: &RoutingTarget, _prompt: &Prompt) -> Result<ExecutionResult> {
        match self.next()? {
            MockResponse::Generate(result) => Ok(ExecutionResult {
                provider_id: target.provider_name.clone(),
                model_id: target.service_id.clone(),
                result,
                latency_ms: 1,
                generation_time_ms: 1,
            }),
            MockResponse::Stream(_) => Err(BitrouterError::internal(
                "MockExecutor: scripted a stream response for a non-streaming call",
            )),
            MockResponse::Error(e) => Err(e),
        }
    }

    async fn execute_stream(
        &self,
        _target: &RoutingTarget,
        _prompt: &Prompt,
    ) -> Result<StreamPartStream> {
        match self.next()? {
            MockResponse::Stream(parts) => {
                let stream = futures::stream::iter(parts.into_iter().map(Ok));
                Ok(Box::pin(stream))
            }
            MockResponse::Generate(_) => Err(BitrouterError::internal(
                "MockExecutor: scripted a non-streaming response for a streaming call",
            )),
            MockResponse::Error(e) => Err(e),
        }
    }
}
