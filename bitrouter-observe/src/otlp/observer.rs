//! [`OtlpObserver`] — an observer that builds OpenTelemetry GenAI spans from
//! BitRouter callback events and dispatches them through the
//! [`super::pipeline::Pipeline`] to one or more configured destinations.
//!
//! Composes via the existing `CompositeObserver`:
//!
//! ```ignore
//! let otlp = Arc::new(OtlpObserver::new(pipeline));
//! let composite = CompositeObserver::new(
//!     vec![spend_observer, metrics, otlp.clone() as Arc<dyn ObserveCallback>],
//!     vec![tool_observer, metrics, otlp.clone() as Arc<dyn ToolObserveCallback>],
//!     vec![metrics, otlp as Arc<dyn AgentObserveCallback>],
//! );
//! ```

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{
    AgentObserveCallback, AgentRequestContext, AgentTurnFailureEvent, AgentTurnSuccessEvent,
    CallerContext, ObserveCallback, RequestContext, RequestFailureEvent, RequestSuccessEvent,
    ToolCallFailureEvent, ToolCallSuccessEvent, ToolObserveCallback, ToolRequestContext,
};

use super::pipeline::Pipeline;
use super::semconv;
use super::span::Span;

/// Observer that exports BitRouter events as OpenTelemetry GenAI spans.
#[derive(Debug, Clone)]
pub struct OtlpObserver {
    pipeline: Pipeline,
}

impl OtlpObserver {
    pub fn new(pipeline: Pipeline) -> Self {
        Self { pipeline }
    }

    fn caller_attrs(span: &mut Span, caller: &CallerContext) {
        span.set_opt(semconv::BR_ACCOUNT_ID, caller.account_id.clone());
        span.set_opt(semconv::BR_KEY_ID, caller.key_id.clone());
        span.set_opt(semconv::BR_POLICY_ID, caller.policy_id.clone());
        if let Some(account_id) = &caller.account_id {
            // OpenRouter compat: also surface as `user.id` when no explicit
            // user identifier was provided. The HTTP path's session extractor
            // overrides this when `X-Bitrouter-User-Id` is present (sets the
            // attribute directly on the trace context before the span is built).
            span.set(semconv::USER_ID, account_id.clone());
        }
    }

    fn build_chat_span(ctx: &RequestContext) -> Span {
        let mut span = Span::new(semconv::span_name_chat(&ctx.model));
        span.set(semconv::OPERATION_NAME, semconv::OP_CHAT);
        span.set(semconv::PROVIDER_NAME, ctx.provider.clone());
        span.set(semconv::REQUEST_MODEL, ctx.model.clone());
        span.set(semconv::BR_ROUTE, ctx.route.clone());
        span.set(semconv::BR_LATENCY_MS, ctx.latency_ms);
        Self::caller_attrs(&mut span, &ctx.caller);
        span
    }

    fn build_tool_span(ctx: &ToolRequestContext) -> Span {
        let mut span = Span::new(semconv::span_name_execute_tool(&ctx.operation));
        span.set(semconv::OPERATION_NAME, semconv::OP_EXECUTE_TOOL);
        span.set(semconv::PROVIDER_NAME, ctx.provider.clone());
        span.set(semconv::BR_LATENCY_MS, ctx.latency_ms);
        Self::caller_attrs(&mut span, &ctx.caller);
        span
    }

    fn build_agent_span(ctx: &AgentRequestContext) -> Span {
        let mut span = Span::new(semconv::span_name_invoke_agent(&ctx.agent_name));
        span.set(semconv::OPERATION_NAME, semconv::OP_INVOKE_AGENT);
        span.set(semconv::PROVIDER_NAME, ctx.protocol.clone());
        span.set(semconv::AGENT_NAME, ctx.agent_name.clone());
        span.set(semconv::BR_LATENCY_MS, ctx.latency_ms);
        if let Some(sid) = &ctx.session_id {
            span.set(semconv::CONVERSATION_ID, sid.clone());
            // OpenRouter compat duplicate.
            span.set(semconv::SESSION_ID, sid.clone());
        }
        Self::caller_attrs(&mut span, &ctx.caller);
        span
    }
}

impl ObserveCallback for OtlpObserver {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut span = Self::build_chat_span(&event.ctx);
            span.set_opt(semconv::USAGE_INPUT_TOKENS, event.usage.input_tokens.total);
            span.set_opt(
                semconv::USAGE_OUTPUT_TOKENS,
                event.usage.output_tokens.total,
            );
            span.set_opt(
                semconv::USAGE_CACHE_READ_INPUT_TOKENS,
                event.usage.input_tokens.cache_read,
            );
            span.set_opt(
                semconv::USAGE_CACHE_CREATION_INPUT_TOKENS,
                event.usage.input_tokens.cache_write,
            );
            span.set_opt(
                semconv::USAGE_REASONING_OUTPUT_TOKENS,
                event.usage.output_tokens.reasoning,
            );

            self.pipeline.dispatch(span);
        })
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut span = Self::build_chat_span(&event.ctx);
            span.set(semconv::ERROR_TYPE, error_variant_name(&event.error));
            self.pipeline.dispatch(span);
        })
    }
}

impl ToolObserveCallback for OtlpObserver {
    fn on_tool_call_success(
        &self,
        event: ToolCallSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let span = Self::build_tool_span(&event.ctx);
            self.pipeline.dispatch(span);
        })
    }

    fn on_tool_call_failure(
        &self,
        event: ToolCallFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut span = Self::build_tool_span(&event.ctx);
            span.set(semconv::ERROR_TYPE, event.error.clone());
            self.pipeline.dispatch(span);
        })
    }
}

impl AgentObserveCallback for OtlpObserver {
    fn on_agent_turn_success(
        &self,
        event: AgentTurnSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let span = Self::build_agent_span(&event.ctx);
            self.pipeline.dispatch(span);
        })
    }

    fn on_agent_turn_failure(
        &self,
        event: AgentTurnFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let mut span = Self::build_agent_span(&event.ctx);
            span.set(semconv::ERROR_TYPE, event.error.clone());
            self.pipeline.dispatch(span);
        })
    }
}

// Mirrors the helper in `model_observer.rs` — kept private rather than
// re-exported because it's a translation detail, not part of the API.
fn error_variant_name(error: &bitrouter_core::errors::BitrouterError) -> String {
    use bitrouter_core::errors::BitrouterError;
    match error {
        BitrouterError::UnsupportedFeature { .. } => "UnsupportedFeature".into(),
        BitrouterError::Cancelled { .. } => "Cancelled".into(),
        BitrouterError::InvalidRequest { .. } => "InvalidRequest".into(),
        BitrouterError::Transport { .. } => "Transport".into(),
        BitrouterError::ResponseDecode { .. } => "ResponseDecode".into(),
        BitrouterError::InvalidResponse { .. } => "InvalidResponse".into(),
        BitrouterError::Provider { .. } => "Provider".into(),
        BitrouterError::StreamProtocol { .. } => "StreamProtocol".into(),
        BitrouterError::AccessDenied { .. } => "AccessDenied".into(),
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_core::errors::BitrouterError;
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };

    use super::super::pipeline::{CaptureTier, Destination, PipelineConfig, Sampling};
    use super::*;
    use std::collections::HashMap;

    fn caller() -> CallerContext {
        CallerContext {
            account_id: Some("acct-1".into()),
            key_id: Some("key-1".into()),
            policy_id: Some("pol-1".into()),
            ..CallerContext::default()
        }
    }

    fn dest() -> Destination {
        Destination {
            name: "test".into(),
            endpoint: "https://example.com/v1/traces".into(),
            headers: HashMap::new(),
            sampling: Sampling::default(),
            redact: vec![],
        }
    }

    fn observer() -> OtlpObserver {
        OtlpObserver::new(Pipeline::new(PipelineConfig {
            capture_tier: CaptureTier::Metadata,
            destinations: vec![dest()],
        }))
    }

    #[tokio::test]
    async fn chat_success_builds_span_with_required_attrs() {
        let obs = observer();
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(1000),
                no_cache: None,
                cache_read: Some(200),
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(500),
                text: None,
                reasoning: Some(50),
            },
            raw: None,
        };
        let event = RequestSuccessEvent {
            ctx: RequestContext {
                route: "fast".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                caller: caller(),
                latency_ms: 250,
            },
            usage,
        };
        // Validate that build_chat_span composes the expected attribute set;
        // we exercise the helper directly to inspect the span pre-dispatch.
        let mut span = OtlpObserver::build_chat_span(&event.ctx);
        span.set_opt(semconv::USAGE_INPUT_TOKENS, event.usage.input_tokens.total);
        span.set_opt(
            semconv::USAGE_OUTPUT_TOKENS,
            event.usage.output_tokens.total,
        );

        assert_eq!(span.name, "chat gpt-4o");
        assert_eq!(span.string_attr(semconv::OPERATION_NAME), Some("chat"));
        assert_eq!(span.string_attr(semconv::PROVIDER_NAME), Some("openai"));
        assert_eq!(span.string_attr(semconv::REQUEST_MODEL), Some("gpt-4o"));
        assert_eq!(span.string_attr(semconv::BR_ROUTE), Some("fast"));
        assert_eq!(span.string_attr(semconv::BR_ACCOUNT_ID), Some("acct-1"));
        assert_eq!(span.string_attr(semconv::USER_ID), Some("acct-1"));

        // Smoke-test the actual callback, ensuring dispatch runs.
        obs.on_request_success(event).await;
    }

    #[tokio::test]
    async fn chat_failure_attaches_error_type() {
        let obs = observer();
        let event = RequestFailureEvent {
            ctx: RequestContext {
                route: "fast".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                caller: caller(),
                latency_ms: 50,
            },
            error: BitrouterError::transport(None, "connection refused"),
        };
        // Run the actual failure path.
        obs.on_request_failure(event.clone()).await;

        // Independently rebuild the span the observer would have dispatched
        // and assert the failure-specific attribute landed.
        let mut span = OtlpObserver::build_chat_span(&event.ctx);
        span.set(semconv::ERROR_TYPE, error_variant_name(&event.error));
        assert_eq!(span.string_attr(semconv::ERROR_TYPE), Some("Transport"));
    }

    #[tokio::test]
    async fn agent_span_includes_session_id_as_conversation_id() {
        let obs = observer();
        let event = AgentTurnSuccessEvent {
            ctx: AgentRequestContext {
                agent_name: "claude-code".into(),
                protocol: "acp".into(),
                session_id: Some("sess-abc".into()),
                caller: caller(),
                latency_ms: 1234,
            },
        };
        let span = OtlpObserver::build_agent_span(&event.ctx);
        assert_eq!(span.name, "invoke_agent claude-code");
        assert_eq!(span.string_attr(semconv::CONVERSATION_ID), Some("sess-abc"));
        assert_eq!(span.string_attr(semconv::SESSION_ID), Some("sess-abc"));
        obs.on_agent_turn_success(event).await;
    }
}
