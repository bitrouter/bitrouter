pub mod admin;
pub mod agents;
pub mod agentskills;
pub mod anthropic;
pub(crate) mod context;
pub mod google;
pub mod mcp;
pub mod models;
pub mod openai;
pub mod routes;
pub(crate) mod sse;
pub mod tools;

mod observe_ctx {
    use std::sync::Arc;
    use std::time::Instant;

    use bitrouter_core::observe::{CallerContext, ObserveCallback};
    use bitrouter_core::routers::routing_table::RoutingTarget;

    /// Bundles observation-related context passed through streaming handlers.
    ///
    /// Created at the call site and consumed inside `handle_stream_with_observe`
    /// to emit success/failure observation events after the stream completes.
    pub(crate) struct StreamObserveContext {
        pub observer: Arc<dyn ObserveCallback>,
        pub route: String,
        pub provider: String,
        pub target_model: String,
        pub caller: CallerContext,
        pub start: Instant,
        /// Stable per-request correlation id.
        pub request_id: String,
        /// Opaque per-request metadata (see [`bitrouter_core::observe::MetadataHook`]).
        pub metadata: serde_json::Value,
        /// Target that returned the committed stream.
        pub executed_target: Option<RoutingTarget>,
    }
}

pub(crate) use observe_ctx::StreamObserveContext;

mod request_log {
    use bitrouter_core::errors::BitrouterError;
    use bitrouter_core::observe::CallerContext;

    /// Emit the canonical "request received" INFO log after model name
    /// resolution has succeeded. Companion to the "request finished" log
    /// emitted by `ModelSpendObserver`.
    pub(crate) fn received(
        caller: &CallerContext,
        route: &str,
        provider: &str,
        model: &str,
        stream: bool,
    ) {
        tracing::info!(
            account_id = caller.account_id.as_deref().unwrap_or("-"),
            route,
            provider,
            model,
            stream,
            "request received",
        );
    }

    /// Emit the "request received" log when model name resolution fails.
    /// The request never reaches the observer, so this is the only log line
    /// the operator will see for these requests.
    pub(crate) fn resolve_failed(caller: &CallerContext, route: &str, error: &BitrouterError) {
        tracing::info!(
            account_id = caller.account_id.as_deref().unwrap_or("-"),
            route,
            error = %error,
            "request received (model resolution failed)",
        );
    }
}

pub(crate) use request_log::{
    received as log_request_received, resolve_failed as log_request_resolve_failed,
};

mod stream_observation {
    use bitrouter_core::{
        errors::BitrouterError,
        models::language::{
            stream_part::LanguageModelStreamPart,
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
    };

    /// Bytes-per-token estimate used when the client disconnects before the
    /// upstream's authoritative Finish event lands. 4 is the canonical
    /// English-text heuristic (≈4 bytes per BPE token); biased low for
    /// non-Latin scripts and high for code/whitespace-heavy output. This
    /// matters only for partial-disconnect billing, where any estimate is
    /// better than the previous behaviour of charging zero.
    const ESTIMATE_BYTES_PER_TOKEN: usize = 4;

    pub(crate) struct StreamObservation {
        usage: Option<LanguageModelUsage>,
        saw_output: bool,
        /// Sum of `delta.len()` bytes seen across TextDelta /
        /// ReasoningDelta / ToolInputDelta. Used to estimate output
        /// tokens when the stream is interrupted before Finish lands —
        /// see `outcome`'s disconnect branch.
        output_bytes: usize,
        error: Option<BitrouterError>,
    }

    impl StreamObservation {
        pub(crate) fn new() -> Self {
            Self {
                usage: None,
                saw_output: false,
                output_bytes: 0,
                error: None,
            }
        }

        pub(crate) fn record_part(&mut self, part: &LanguageModelStreamPart) {
            match part {
                LanguageModelStreamPart::Finish { usage, .. } => {
                    self.usage = Some(usage.clone());
                }
                LanguageModelStreamPart::Error { error } => {
                    self.error = Some(BitrouterError::stream_protocol(
                        None,
                        "upstream stream emitted an error part",
                        Some(error.clone()),
                    ));
                }
                LanguageModelStreamPart::TextDelta { delta, .. }
                | LanguageModelStreamPart::ReasoningDelta { delta, .. }
                | LanguageModelStreamPart::ToolInputDelta { delta, .. } => {
                    self.saw_output = true;
                    self.output_bytes = self.output_bytes.saturating_add(delta.len());
                }
                LanguageModelStreamPart::ToolCall { .. }
                | LanguageModelStreamPart::ToolInputStart { .. }
                | LanguageModelStreamPart::ToolInputEnd { .. }
                | LanguageModelStreamPart::File { .. }
                | LanguageModelStreamPart::ToolApprovalRequest { .. }
                | LanguageModelStreamPart::UrlSource { .. }
                | LanguageModelStreamPart::DocumentSource { .. }
                | LanguageModelStreamPart::ToolResult { .. } => {
                    self.saw_output = true;
                }
                _ => {}
            }
        }

        pub(crate) fn outcome(
            self,
            client_disconnected: bool,
        ) -> std::result::Result<LanguageModelUsage, BitrouterError> {
            // Authoritative usage from a Finish event always wins —
            // even when the client disconnected immediately after EOS,
            // the upstream already computed and shipped us the tokens.
            if let Some(usage) = self.usage {
                return Ok(usage);
            }
            // Mid-stream disconnect with no Finish: bill what we can
            // estimate from delta bytes seen so the request isn't free
            // for an attacker who drains the response and then hangs
            // up. Input tokens are unknown at this layer; the missing
            // input cost is a known gap (would need the prompt token
            // count plumbed in from the request path).
            if client_disconnected {
                // Saturate at u32::MAX rather than wrap, in the
                // pathological case where output_bytes would overflow.
                let estimated_output =
                    u32::try_from(self.output_bytes.div_ceil(ESTIMATE_BYTES_PER_TOKEN))
                        .unwrap_or(u32::MAX);
                return Ok(synthesize_usage_for_partial(estimated_output));
            }
            if let Some(error) = self.error {
                return Err(error);
            }
            if self.saw_output {
                return Ok(empty_usage());
            }
            Err(BitrouterError::stream_protocol(
                None,
                "stream completed without finish event",
                None,
            ))
        }
    }

    fn empty_usage() -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: None,
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: None,
                text: None,
                reasoning: None,
            },
            raw: None,
        }
    }

    fn synthesize_usage_for_partial(output_tokens: u32) -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: None,
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(output_tokens),
                text: Some(output_tokens),
                reasoning: None,
            },
            raw: None,
        }
    }
}

pub(crate) use stream_observation::StreamObservation;

#[cfg(test)]
mod stream_observation_tests {
    use bitrouter_core::models::language::{
        finish_reason::LanguageModelFinishReason,
        stream_part::LanguageModelStreamPart,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    };

    use super::StreamObservation;

    fn finish_usage() -> LanguageModelUsage {
        LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(123),
                no_cache: Some(123),
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(456),
                text: Some(456),
                reasoning: None,
            },
            raw: None,
        }
    }

    #[test]
    fn finish_before_disconnect_returns_authoritative_usage() {
        // Even when the client disconnects, a Finish event that landed
        // first is the source of truth — bill the full upstream-reported
        // usage rather than a delta-byte estimate.
        let mut obs = StreamObservation::new();
        obs.record_part(&LanguageModelStreamPart::TextDelta {
            id: "0".into(),
            delta: "hi".into(),
            provider_metadata: None,
        });
        obs.record_part(&LanguageModelStreamPart::Finish {
            usage: finish_usage(),
            finish_reason: LanguageModelFinishReason::Stop,
            provider_metadata: None,
        });
        let usage = obs.outcome(true).expect("finish wins over disconnect");
        assert_eq!(usage.output_tokens.total, Some(456));
        assert_eq!(usage.input_tokens.total, Some(123));
    }

    #[test]
    fn disconnect_without_finish_estimates_output_from_delta_bytes() {
        // Audit B8: a client that drains output then disconnects before
        // EOS used to be free. Now: estimate output tokens from streamed
        // bytes (4 b/tok heuristic) so the request bills something.
        let mut obs = StreamObservation::new();
        // 16 bytes ⇒ 4 estimated tokens (16/4 ceil).
        obs.record_part(&LanguageModelStreamPart::TextDelta {
            id: "0".into(),
            delta: "1234567890123456".into(),
            provider_metadata: None,
        });
        let usage = obs.outcome(true).expect("disconnect with output");
        assert_eq!(usage.output_tokens.text, Some(4));
        assert_eq!(usage.output_tokens.total, Some(4));
        // Input billing on disconnect is a known gap (prompt not plumbed
        // through StreamObservation); the field stays None.
        assert_eq!(usage.input_tokens.total, None);
    }

    #[test]
    fn disconnect_with_no_output_estimates_zero() {
        // Disconnect before any delta = nothing to bill. The synthesized
        // usage is all-zero, which calculate_cost handles as Some(0.0).
        let obs = StreamObservation::new();
        let usage = obs.outcome(true).expect("disconnect with no output");
        assert_eq!(usage.output_tokens.total, Some(0));
    }
}
