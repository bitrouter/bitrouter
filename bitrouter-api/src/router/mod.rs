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
    }
}

pub(crate) use observe_ctx::StreamObserveContext;

mod stream_observation {
    use bitrouter_core::{
        errors::BitrouterError,
        models::language::{
            stream_part::LanguageModelStreamPart,
            usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
        },
    };

    pub(crate) struct StreamObservation {
        usage: Option<LanguageModelUsage>,
        saw_output: bool,
        error: Option<BitrouterError>,
    }

    impl StreamObservation {
        pub(crate) fn new() -> Self {
            Self {
                usage: None,
                saw_output: false,
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
                LanguageModelStreamPart::TextDelta { .. }
                | LanguageModelStreamPart::ToolCall { .. }
                | LanguageModelStreamPart::ToolInputStart { .. }
                | LanguageModelStreamPart::ToolInputDelta { .. }
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
            if client_disconnected {
                return Err(BitrouterError::cancelled(
                    None,
                    "client disconnected during stream",
                ));
            }
            if let Some(error) = self.error {
                return Err(error);
            }
            if let Some(usage) = self.usage {
                return Ok(usage);
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
}

pub(crate) use stream_observation::StreamObservation;
