//! OpenTelemetry metrics with multi-tenant dimensions.
//!
//! Owns a private [`SdkMeterProvider`] (we don't install one globally — that
//! would clobber any other consumer of the OTel globals in the process).
//! Shutdown happens explicitly via [`OtelMetrics::shutdown`], not in `Drop`,
//! because `Drop` would race the tokio runtime if the exporter still has
//! in-flight work.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use bitrouter_sdk::language_model::{PipelineContext, RequestOutcome, StreamPart};

use crate::otel::cardinality::CardinalityLimiter;
use crate::otel::config::OtelConfig;

/// OpenTelemetry metrics with multi-tenant attribution.
pub struct OtelMetrics {
    provider: SdkMeterProvider,

    request_counter: Counter<u64>,
    latency_histogram: Histogram<f64>,
    /// GenAI-semconv `gen_ai.client.token.usage` — a single histogram;
    /// input vs. output is distinguished by the `gen_ai.token.type`
    /// attribute, never by a second same-named instrument.
    token_usage: Histogram<u64>,
    error_counter: Counter<u64>,
    stream_parts_counter: Counter<u64>,

    api_key_limiter: Arc<CardinalityLimiter>,
    user_id_limiter: Arc<CardinalityLimiter>,
}

impl OtelMetrics {
    /// Create new metrics with the given configuration.
    pub fn new(
        config: &OtelConfig,
        resource: Resource,
        api_key_limiter: Arc<CardinalityLimiter>,
        user_id_limiter: Arc<CardinalityLimiter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Transport (OTLP/HTTP vs OTLP/gRPC) is chosen at compile time by the
        // crate's feature flags — see `crate::otel::transport`.
        let exporter = crate::otel::transport::metric_exporter(config)?;

        let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_interval(Duration::from_millis(config.metrics.export_interval_ms))
            .build();

        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();

        let meter: Meter = provider.meter("bitrouter");

        let request_counter = meter
            .u64_counter("bitrouter.requests")
            .with_description("Total requests processed")
            .with_unit("1")
            .build();
        let latency_histogram = meter
            .f64_histogram("gen_ai.client.operation.duration")
            .with_description("GenAI client operation duration")
            .with_unit("s")
            .build();
        // Per the GenAI semconv, `gen_ai.client.token.usage` is a single
        // histogram; input vs. output is a `gen_ai.token.type` attribute.
        // Registering it twice (once per direction) is a duplicate-instrument
        // conflict the SDK warns about and merges anyway.
        let token_usage = meter
            .u64_histogram("gen_ai.client.token.usage")
            .with_description("Number of tokens used, by token type")
            .with_unit("{token}")
            .build();
        let error_counter = meter
            .u64_counter("bitrouter.errors")
            .with_description("Total errors encountered")
            .with_unit("1")
            .build();
        let stream_parts_counter = meter
            .u64_counter("bitrouter.stream_parts")
            .with_description("Total stream parts processed")
            .with_unit("1")
            .build();

        Ok(Self {
            provider,
            request_counter,
            latency_histogram,
            token_usage,
            error_counter,
            stream_parts_counter,
            api_key_limiter,
            user_id_limiter,
        })
    }

    /// Record a completed request.
    pub fn record_request(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let mut attributes = self.build_base_attributes(ctx);

        let outcome_str = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        attributes.push(KeyValue::new("outcome", outcome_str));

        if let Some(result) = &ctx.execution_result {
            // GenAI semconv: `gen_ai.provider.name` (replaces the older
            // `gen_ai.system`). The same attribute vocabulary is shared
            // across traces and metrics, so this metric dimension follows
            // the trace-side rename.
            attributes.push(KeyValue::new(
                "gen_ai.provider.name",
                result.provider_id.clone(),
            ));
            attributes.push(KeyValue::new(
                "gen_ai.response.model",
                result.model_id.clone(),
            ));
            if let Some(label) = &result.account_label {
                attributes.push(KeyValue::new("bitrouter.account_label", label.clone()));
            }

            // GenAI semconv: operation.duration is a histogram in seconds.
            self.latency_histogram
                .record(result.latency_ms as f64 / 1000.0, &attributes);

            if let Some(usage) = &result.result.usage {
                let mut input_attrs = attributes.clone();
                input_attrs.push(KeyValue::new("gen_ai.token.type", "input"));
                self.token_usage.record(usage.prompt_tokens, &input_attrs);

                let mut output_attrs = attributes.clone();
                output_attrs.push(KeyValue::new("gen_ai.token.type", "output"));
                self.token_usage
                    .record(usage.completion_tokens, &output_attrs);
            }
        }

        self.request_counter.add(1, &attributes);

        if matches!(outcome, RequestOutcome::Failed(_)) {
            self.error_counter.add(1, &attributes);
        }
    }

    /// Record a stream part.
    pub fn record_stream_part(&self, part: &StreamPart) {
        self.stream_parts_counter
            .add(1, &[KeyValue::new("part_type", stream_part_type(part))]);
    }

    /// Flush pending metrics and shut down the meter provider. The caller
    /// drives this — see the note on [`OtelMetrics`].
    pub fn shutdown(&self) {
        // Both calls are best-effort; surfacing an error here would just
        // double-log the SDK's own warning.
        let _ = self.provider.force_flush();
        let _ = self.provider.shutdown();
    }

    fn build_base_attributes(&self, ctx: &PipelineContext) -> Vec<KeyValue> {
        let caller = ctx.caller();
        vec![
            KeyValue::new("api_key_id", self.api_key_limiter.cap(caller.api_key_id())),
            KeyValue::new("user_id", self.user_id_limiter.cap(caller.user_id())),
        ]
    }
}

fn stream_part_type(part: &StreamPart) -> &'static str {
    match part {
        StreamPart::TextDelta { .. } => "text_delta",
        StreamPart::ReasoningDelta { .. } => "reasoning_delta",
        StreamPart::ToolCallDelta { .. } => "tool_call_delta",
        StreamPart::Usage { .. } => "usage",
        StreamPart::ResponseStarted { .. } => "response_started",
        StreamPart::Finish { .. } => "finish",
        StreamPart::ResponseCompleted { .. } => "response_completed",
    }
}
