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
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;

use bitrouter_sdk::language_model::{PipelineContext, RequestOutcome, StreamPart};

use crate::otel::cardinality::CardinalityLimiter;
use crate::otel::config::OtelConfig;
use crate::otel::processor_runtime::ProcessorRuntime;

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

        let reader = PeriodicReader::builder(exporter, ProcessorRuntime::new())
            .with_interval(Duration::from_millis(config.metrics.export_interval_ms))
            .build();

        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();

        Ok(Self::from_provider(
            provider,
            api_key_limiter,
            user_id_limiter,
        ))
    }

    /// Register the instruments on an already-built provider.
    ///
    /// Split from [`OtelMetrics::new`] so the reader is a parameter rather
    /// than a hard-wired OTLP `PeriodicReader`: the conformance test below
    /// drives a real request lifecycle through a capturing exporter and checks
    /// what came out against `bitrouter_sdk::observe::schema`, which is not
    /// possible against a provider that can only talk to a collector.
    fn from_provider(
        provider: SdkMeterProvider,
        api_key_limiter: Arc<CardinalityLimiter>,
        user_id_limiter: Arc<CardinalityLimiter>,
    ) -> Self {
        // wire-visible: do not rename — the meter name is exported as the
        // instrumentation scope on every metric point, so alerting rules and
        // dashboards filter on it. It is independent of the crate that hosts
        // this module.
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

        Self {
            provider,
            request_counter,
            latency_histogram,
            token_usage,
            error_counter,
            stream_parts_counter,
            api_key_limiter,
            user_id_limiter,
        }
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

        // GenAI semconv: operation.duration is a histogram in seconds. The SDK
        // exposes elapsed time even when an early failure produced no
        // `ExecutionResult`, so failed preflight requests are included too.
        self.latency_histogram
            .record(request_duration_seconds(ctx), &attributes);

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

fn request_duration_seconds(ctx: &PipelineContext) -> f64 {
    ctx.request_duration_ms() as f64 / 1000.0
}

fn stream_part_type(part: &StreamPart) -> &'static str {
    match part {
        StreamPart::TextStart { .. } => "text_start",
        StreamPart::TextDelta { .. } => "text_delta",
        StreamPart::TextEnd { .. } => "text_end",
        StreamPart::ReasoningStart { .. } => "reasoning_start",
        StreamPart::ReasoningDelta { .. } => "reasoning_delta",
        StreamPart::ReasoningEnd { .. } => "reasoning_end",
        StreamPart::ToolCallDelta { .. } => "tool_call_delta",
        StreamPart::ServerToolCall { .. } => "server_tool_call",
        StreamPart::ServerToolResult { .. } => "server_tool_result",
        StreamPart::File { .. } => "file",
        StreamPart::Source { .. } => "source",
        StreamPart::Usage { .. } => "usage",
        StreamPart::ResponseStarted { .. } => "response_started",
        StreamPart::Finish { .. } => "finish",
        StreamPart::ResponseCompleted { .. } => "response_completed",
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::{
        ExecutionResult, GenerateResult, GenerationParams, PipelineRequest, Prompt,
    };

    use super::*;

    #[test]
    fn request_latency_histogram_uses_finalized_milliseconds_as_seconds() {
        let prompt = Prompt {
            model: "test-model".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let request =
            PipelineRequest::new("test-model", CallerContext::new("api-key", "user"), prompt);
        let mut ctx = PipelineContext::new(request);
        ctx.execution_result = Some(ExecutionResult {
            provider_id: "openai".into(),
            model_id: "test-model".into(),
            account_label: None,
            result: GenerateResult {
                content: Vec::new(),
                usage: None,
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
            request_duration_ms: 42,
            upstream_duration_ms: Some(40),
            server_tool_calls: Vec::new(),
        });

        assert_eq!(request_duration_seconds(&ctx), 0.042);
    }

    #[test]
    fn request_latency_histogram_includes_early_failures() {
        let prompt = Prompt {
            model: "test-model".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let request =
            PipelineRequest::new("test-model", CallerContext::new("api-key", "user"), prompt);
        let ctx = PipelineContext::new(request);
        std::thread::sleep(Duration::from_millis(2));

        assert!(request_duration_seconds(&ctx) > 0.0);
    }

    // ── Schema conformance ───────────────────────────────────────────────────

    /// One exported instrument, flattened to the parts the schema declares.
    #[derive(Debug)]
    struct CapturedMetric {
        name: String,
        instrument: &'static str,
        unit: String,
        attribute_keys: Vec<String>,
    }

    /// Captures every exported metric in-process, the way `http_layer`'s
    /// `CapturingProcessor` captures spans: assertions read the SDK's own data
    /// model rather than decoding OTLP wire bytes.
    #[derive(Debug, Clone)]
    struct CapturingMetricExporter {
        captured: Arc<std::sync::Mutex<Vec<CapturedMetric>>>,
    }

    /// The instrument kind, spelled as `otel::schema` spells it, plus every
    /// attribute key on every data point.
    fn kind_and_attribute_keys<T>(
        data: &opentelemetry_sdk::metrics::data::MetricData<T>,
    ) -> (&'static str, Vec<String>) {
        use opentelemetry_sdk::metrics::data::MetricData;
        match data {
            MetricData::Sum(sum) => (
                "counter",
                sum.data_points()
                    .flat_map(|point| point.attributes().map(|kv| kv.key.to_string()))
                    .collect(),
            ),
            MetricData::Histogram(histogram) => (
                "histogram",
                histogram
                    .data_points()
                    .flat_map(|point| point.attributes().map(|kv| kv.key.to_string()))
                    .collect(),
            ),
            MetricData::Gauge(_) => ("gauge", Vec::new()),
            MetricData::ExponentialHistogram(_) => ("exponential_histogram", Vec::new()),
        }
    }

    impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for CapturingMetricExporter {
        async fn export(
            &self,
            metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            use opentelemetry_sdk::metrics::data::AggregatedMetrics;
            let mut guard = match self.captured.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            for scope in metrics.scope_metrics() {
                for metric in scope.metrics() {
                    let (instrument, attribute_keys) = match metric.data() {
                        AggregatedMetrics::F64(data) => kind_and_attribute_keys(data),
                        AggregatedMetrics::U64(data) => kind_and_attribute_keys(data),
                        AggregatedMetrics::I64(data) => kind_and_attribute_keys(data),
                    };
                    guard.push(CapturedMetric {
                        name: metric.name().to_string(),
                        instrument,
                        unit: metric.unit().to_string(),
                        attribute_keys,
                    });
                }
            }
            Ok(())
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _timeout: Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
            opentelemetry_sdk::metrics::Temporality::Cumulative
        }
    }

    /// A context carrying everything the conditional dimensions key off, so one
    /// lifecycle exercises the widest attribute set the recorder can produce.
    fn fully_attributed_context() -> PipelineContext {
        let prompt = Prompt {
            model: "test-model".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: true,
        };
        let request =
            PipelineRequest::new("test-model", CallerContext::new("api-key", "user"), prompt);
        let mut ctx = PipelineContext::new(request);
        ctx.execution_result = Some(ExecutionResult {
            provider_id: "openai".into(),
            model_id: "test-model".into(),
            account_label: Some("primary".into()),
            result: GenerateResult {
                content: Vec::new(),
                usage: Some(bitrouter_sdk::language_model::Usage {
                    prompt_tokens: 11,
                    completion_tokens: 7,
                    ..Default::default()
                }),
                finish_reason: None,
                response_id: None,
                stop_details: None,
                provider_metadata: Default::default(),
            },
            request_duration_ms: 42,
            upstream_duration_ms: Some(40),
            server_tool_calls: Vec::new(),
        });
        ctx
    }

    /// The metrics half of the committed schema had no enforcement at all:
    /// `SCHEMA.metrics` declared five instruments and their dimensions while
    /// this module hardcoded both, so a rename here — the thing the
    /// `wire-visible-names` invariant says breaks every dashboard silently —
    /// would have passed every gate and left `span-schema.json` promising a
    /// series nobody emits. This is the span suite's rule applied to metrics,
    /// and it runs in both directions because, unlike span attributes, the
    /// instrument set is small enough to exercise completely in one lifecycle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emitted_metrics_conform_to_the_committed_schema() {
        use bitrouter_sdk::observe::schema::{Instrument, Requirement, SCHEMA};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader = PeriodicReader::builder(
            CapturingMetricExporter {
                captured: Arc::clone(&captured),
            },
            ProcessorRuntime::new(),
        )
        // Long enough that the only export is the one `shutdown` forces, so
        // the capture is a single deterministic snapshot.
        .with_interval(Duration::from_secs(3600))
        .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = OtelMetrics::from_provider(
            provider,
            Arc::new(CardinalityLimiter::new(16)),
            Arc::new(CardinalityLimiter::new(16)),
        );

        let ctx = fully_attributed_context();
        metrics.record_request(&ctx, &RequestOutcome::Completed);
        // The error counter only moves on a failure, and `bitrouter.errors` is
        // declared, so without this the "every declared metric was emitted"
        // half would fail on a real gap in the drive rather than a real gap in
        // the schema.
        metrics.record_request(
            &ctx,
            &RequestOutcome::Failed(bitrouter_sdk::error::BitrouterError::Unauthorized(
                "test".into(),
            )),
        );
        metrics.record_stream_part(&StreamPart::TextEnd { id: "0".into() });
        metrics.shutdown();

        let captured = match captured.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert!(
            !captured.is_empty(),
            "conformance over an empty metric set passes vacuously"
        );

        for metric in captured.iter() {
            let def = SCHEMA
                .metrics
                .iter()
                .find(|def| def.name == metric.name)
                .unwrap_or_else(|| {
                    panic!(
                        "instrument `{}` is exported but not declared in otel::schema — \
                         instrument names are wire contract (`wire-visible-names`); declare it \
                         and regenerate crates/bitrouter-sdk/span-schema.json, or stop emitting \
                         it",
                        metric.name
                    )
                });
            let declared_kind = match def.instrument {
                Instrument::Counter => "counter",
                Instrument::Histogram => "histogram",
            };
            assert_eq!(
                metric.instrument, declared_kind,
                "`{}` is exported as a {} but otel::schema declares a {declared_kind} — the \
                 kind decides what a backend may ask of the series",
                metric.name, metric.instrument
            );
            assert_eq!(
                metric.unit, def.unit,
                "`{}` is exported with unit `{}` but otel::schema declares `{}`",
                metric.name, metric.unit, def.unit
            );
            for key in &metric.attribute_keys {
                assert!(
                    def.dimensions.iter().any(|dim| dim.key == key),
                    "`{}` carries dimension `{key}`, which otel::schema does not declare on it \
                     — every dimension is a cardinality decision, so adding one is a schema \
                     change",
                    metric.name
                );
            }
            for dim in def
                .dimensions
                .iter()
                .filter(|dim| matches!(dim.requirement, Requirement::Required))
            {
                assert!(
                    metric.attribute_keys.iter().any(|key| key == dim.key),
                    "`{}` is missing dimension `{}`, which otel::schema declares as required",
                    metric.name,
                    dim.key
                );
            }
        }

        for def in SCHEMA.metrics {
            assert!(
                captured.iter().any(|metric| metric.name == def.name),
                "otel::schema declares `{}` but a full request lifecycle emitted no such \
                 instrument — a declared-but-dead series is what the committed artifact \
                 promises to anyone implementing against it",
                def.name
            );
        }
    }
}
