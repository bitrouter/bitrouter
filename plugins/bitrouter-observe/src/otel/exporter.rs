//! OpenTelemetry exporter implementation with multi-tenant attribution.
//!
//! Two spans per request:
//! - `chat {model}` (root, `SERVER`) — carries the full GenAI semconv set.
//! - `bitrouter.execution` (child, `CLIENT`) — routing details.
//!
//! Span names follow the GenAI semconv: `{gen_ai.operation.name} {gen_ai.request.model}`.
//!
//! W3C `traceparent` propagation is registered at construction (`global::set_text_map_propagator`).
//! Without that, `global::get_text_map_propagator(...)` returns a no-op and inbound trace
//! context is silently dropped — the exact v0 bug the issue called out.
//!
//! In-flight spans are tracked in a [`DashMap`] keyed by request id, not a global
//! `Mutex<HashMap>`. The previous draft held a process-wide mutex across every
//! stream part on the hot path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{
    Context, KeyValue, global,
    propagation::Extractor,
    trace::{Span, SpanKind, Status, TraceContextExt, Tracer},
};
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, RandomIdGenerator, Sampler, Tracer as SdkTracer,
    TracerProvider,
};
use opentelemetry_sdk::{Resource, runtime};
use opentelemetry_semantic_conventions::SCHEMA_URL;
use opentelemetry_semantic_conventions::attribute::{SERVICE_NAME, SERVICE_VERSION};

use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext, StreamInterest, StreamPart,
};

use crate::otel::cardinality::CardinalityLimiter;
use crate::otel::config::{OtelConfig, SamplerKind};

/// HTTP header extractor for W3C trace context propagation.
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

struct SpanEntry {
    context: Context,
    created_at: Instant,
}

/// OpenTelemetry exporter with multi-tenant attribution.
pub struct OtelExporter {
    tracer: SdkTracer,
    provider: TracerProvider,
    metrics: Option<crate::otel::metrics::OtelMetrics>,

    /// Cardinality limiters — applied to *metric* attributes only. Spans
    /// carry raw values: cardinality is a metrics-storage concern, not a
    /// tracing one, and capping spans loses per-tenant debug fidelity.
    api_key_limiter: Arc<CardinalityLimiter>,
    user_id_limiter: Arc<CardinalityLimiter>,

    /// In-flight spans, keyed by request id.
    active_spans: Arc<DashMap<String, SpanEntry>>,

    /// Maximum time to keep a span before automatic cleanup.
    span_timeout: Duration,
}

impl OtelExporter {
    /// Create a new exporter, install the W3C propagator globally, and build
    /// a per-exporter `TracerProvider` (not installed globally — we hand out
    /// our own `BoxedTracer`).
    pub fn new(mut config: OtelConfig) -> Result<Self, Box<dyn std::error::Error>> {
        config = config.with_env_overrides();

        // Install the W3C TraceContext propagator so inbound `traceparent`
        // can actually be extracted. Without this, `get_text_map_propagator`
        // returns a no-op and propagation is silently dropped.
        global::set_text_map_propagator(TraceContextPropagator::new());

        let resource = build_resource(&config);

        let span_exporter: SpanExporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(&config.endpoint)
            .with_headers(config.headers.clone())
            .build()?;

        let batch_config = BatchConfigBuilder::default()
            .with_max_queue_size(config.traces.batch.max_queue_size)
            .with_scheduled_delay(Duration::from_millis(config.traces.batch.flush_ms))
            .build();
        let processor = BatchSpanProcessor::builder(span_exporter, runtime::Tokio)
            .with_batch_config(batch_config)
            .build();

        let provider = TracerProvider::builder()
            .with_span_processor(processor)
            .with_sampler(build_sampler(&config))
            .with_id_generator(RandomIdGenerator::default())
            .with_resource(resource.clone())
            .build();
        let tracer = provider.tracer("bitrouter");

        let api_key_limiter = Arc::new(CardinalityLimiter::new(config.metrics.api_key_id_cap));
        let user_id_limiter = Arc::new(CardinalityLimiter::new(config.metrics.user_id_cap));

        let metrics = if config.metrics.enabled {
            Some(crate::otel::metrics::OtelMetrics::new(
                &config,
                resource,
                Arc::clone(&api_key_limiter),
                Arc::clone(&user_id_limiter),
            )?)
        } else {
            None
        };

        Ok(Self {
            tracer,
            provider,
            metrics,
            api_key_limiter,
            user_id_limiter,
            active_spans: Arc::new(DashMap::new()),
            span_timeout: Duration::from_secs(300),
        })
    }

    /// Cardinality statistics for monitoring.
    pub fn cardinality_stats(&self) -> CardinalityStats {
        CardinalityStats {
            api_key_count: self.api_key_limiter.cardinality(),
            user_id_count: self.user_id_limiter.cardinality(),
        }
    }

    /// Flush and shut down both the tracer and the metric provider. The
    /// caller drives this from app shutdown.
    pub fn shutdown(&self) {
        let _ = self.provider.force_flush();
        let _ = self.provider.shutdown();
        if let Some(m) = &self.metrics {
            m.shutdown();
        }
    }

    fn gc_expired_spans(&self) {
        let now = Instant::now();
        let timeout = self.span_timeout;
        self.active_spans
            .retain(|_, entry| now.duration_since(entry.created_at) < timeout);
    }
}

#[derive(Debug, Clone)]
pub struct CardinalityStats {
    pub api_key_count: usize,
    pub user_id_count: usize,
}

#[async_trait]
impl ObserveHook for OtelExporter {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        match phase {
            Phase::PreRequest => {
                // Extract inbound W3C trace context, if any.
                let parent_context =
                    global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(ctx.headers())));

                let model = ctx.model().to_string();
                // GenAI semconv span name: "{operation} {model}".
                let span_name = format!("chat {model}");

                let attributes = vec![
                    KeyValue::new("bitrouter.request_id", ctx.request_id().to_string()),
                    // Raw values on spans — capping is a metrics concern.
                    KeyValue::new(
                        "bitrouter.api_key_id",
                        ctx.caller().api_key_id().to_string(),
                    ),
                    KeyValue::new("bitrouter.user_id", ctx.caller().user_id().to_string()),
                    // GenAI semconv from the start so the operation is
                    // identifiable even when execution never completes.
                    KeyValue::new("gen_ai.operation.name", "chat"),
                    KeyValue::new("gen_ai.request.model", model),
                ];

                let builder = self
                    .tracer
                    .span_builder(span_name)
                    .with_kind(SpanKind::Server)
                    .with_attributes(attributes);

                let span = if parent_context.span().span_context().is_valid() {
                    builder.start_with_context(&self.tracer, &parent_context)
                } else {
                    builder.start(&self.tracer)
                };

                let cx = Context::current_with_span(span);
                self.active_spans.insert(
                    ctx.request_id().to_string(),
                    SpanEntry {
                        context: cx,
                        created_at: Instant::now(),
                    },
                );
                // Best-effort GC — only walks the map, not held across awaits.
                self.gc_expired_spans();
            }
            Phase::Execution => {
                if let Some(entry) = self.active_spans.get(ctx.request_id()) {
                    let _guard = entry.context.clone().attach();
                    let mut span = self
                        .tracer
                        .span_builder("bitrouter.execution")
                        .with_kind(SpanKind::Client)
                        .start(&self.tracer);

                    if let Some(chain) = &ctx.route_chain {
                        span.set_attribute(KeyValue::new(
                            "bitrouter.route_chain_length",
                            chain.len() as i64,
                        ));
                        if let Some(target) = chain.first() {
                            span.set_attribute(KeyValue::new(
                                "bitrouter.target_provider",
                                target.provider_name.clone(),
                            ));
                            span.set_attribute(KeyValue::new(
                                "bitrouter.target_model",
                                target.service_id.clone(),
                            ));
                        }
                    }
                    span.end();
                }
            }
            _ => {}
        }
    }

    fn stream_interest(&self) -> StreamInterest {
        StreamInterest::all()
    }

    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart) {
        if let Some(metrics) = &self.metrics {
            metrics.record_stream_part(part);
        }

        if let Some(entry) = self.active_spans.get(&ctx.request_id) {
            let _guard = entry.context.clone().attach();
            match part {
                StreamPart::ToolCallDelta {
                    name: Some(name), ..
                } => {
                    opentelemetry::trace::get_active_span(|span| {
                        span.add_event(
                            "tool_call.started",
                            vec![KeyValue::new("tool.name", name.clone())],
                        );
                    });
                }
                StreamPart::Usage { usage } => {
                    opentelemetry::trace::get_active_span(|span| {
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.stream_tokens",
                            usage.total() as i64,
                        ));
                    });
                }
                _ => {}
            }
        }
    }

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        // `DashMap::remove` returns `Option<(K, V)>`.
        let Some((_, entry)) = self.active_spans.remove(ctx.request_id()) else {
            // No matching span — request may have failed before `PreRequest`
            // or been GC'd. Still record metrics below.
            if let Some(metrics) = &self.metrics {
                metrics.record_request(ctx, outcome);
            }
            return;
        };

        let _guard = entry.context.clone().attach();
        opentelemetry::trace::get_active_span(|span| {
            if let Some(result) = &ctx.execution_result {
                span.set_attribute(KeyValue::new(
                    "bitrouter.provider_id",
                    result.provider_id.clone(),
                ));
                span.set_attribute(KeyValue::new("bitrouter.model_id", result.model_id.clone()));
                span.set_attribute(KeyValue::new(
                    "bitrouter.latency_ms",
                    result.latency_ms as i64,
                ));
                span.set_attribute(KeyValue::new(
                    "bitrouter.generation_time_ms",
                    result.generation_time_ms as i64,
                ));
                if let Some(label) = &result.account_label {
                    span.set_attribute(KeyValue::new("bitrouter.account_label", label.clone()));
                }

                // GenAI semconv.
                span.set_attribute(KeyValue::new("gen_ai.system", result.provider_id.clone()));
                span.set_attribute(KeyValue::new(
                    "gen_ai.response.model",
                    result.model_id.clone(),
                ));

                if let Some(usage) = &result.result.usage {
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.input_tokens",
                        usage.prompt_tokens as i64,
                    ));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.usage.output_tokens",
                        usage.completion_tokens as i64,
                    ));
                    if usage.reasoning_tokens > 0 {
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.reasoning_tokens",
                            usage.reasoning_tokens as i64,
                        ));
                    }
                }

                // Spec: gen_ai.response.finish_reasons is an array of strings.
                if let Some(reason) = &result.result.finish_reason {
                    span.set_attribute(KeyValue::new(
                        "gen_ai.response.finish_reasons",
                        opentelemetry::Value::Array(opentelemetry::Array::String(vec![
                            finish_reason_to_str(reason).into(),
                        ])),
                    ));
                }
            }

            match outcome {
                RequestOutcome::Completed => {
                    span.set_status(Status::Ok);
                    span.set_attribute(KeyValue::new("bitrouter.outcome", "completed"));
                }
                RequestOutcome::Failed(err) => {
                    span.set_status(Status::error(err.to_string()));
                    span.set_attribute(KeyValue::new("bitrouter.outcome", "failed"));
                    // OTel exceptions semconv: error.type identifies the
                    // failure class.
                    span.set_attribute(KeyValue::new("error.type", error_type(err)));
                    span.set_attribute(KeyValue::new("error.message", err.to_string()));
                }
                RequestOutcome::ClientDisconnected => {
                    span.set_status(Status::error("client_disconnected"));
                    span.set_attribute(KeyValue::new("bitrouter.outcome", "disconnected"));
                    span.set_attribute(KeyValue::new("error.type", "client_disconnected"));
                }
            }
            span.end();
        });

        if let Some(metrics) = &self.metrics {
            metrics.record_request(ctx, outcome);
        }
    }
}

fn build_resource(config: &OtelConfig) -> Resource {
    let mut attrs = vec![
        KeyValue::new(SERVICE_NAME, config.service_name.clone()),
        KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
    ];
    for (k, v) in &config.resource_attributes {
        // Skip keys we already set explicitly — OTel-spec lets the env var
        // override, but we already merged that into `config.service_name`.
        if k == SERVICE_NAME || k == SERVICE_VERSION {
            continue;
        }
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    Resource::from_schema_url(attrs, SCHEMA_URL)
}

fn build_sampler(config: &OtelConfig) -> Sampler {
    let ratio = config.sampler_arg.unwrap_or(1.0).clamp(0.0, 1.0);
    match config.sampler {
        SamplerKind::AlwaysOn => Sampler::AlwaysOn,
        SamplerKind::AlwaysOff => Sampler::AlwaysOff,
        SamplerKind::TraceIdRatio => Sampler::TraceIdRatioBased(ratio),
        SamplerKind::ParentBasedAlwaysOn => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        SamplerKind::ParentBasedAlwaysOff => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        SamplerKind::ParentBasedTraceIdRatio => {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
        }
    }
}

fn finish_reason_to_str(reason: &bitrouter_sdk::language_model::FinishReason) -> String {
    use bitrouter_sdk::language_model::FinishReason::*;
    match reason {
        Stop => "stop".to_string(),
        Length => "length".to_string(),
        ToolCalls => "tool_calls".to_string(),
        ContentFilter => "content_filter".to_string(),
        Other(s) => s.clone(),
        Error(_) => "error".to_string(),
    }
}

fn error_type(err: &bitrouter_sdk::error::BitrouterError) -> String {
    // BitrouterError variants are unit-ish; the Debug name is the class.
    // We strip the payload so the attribute stays low-cardinality.
    let dbg = format!("{err:?}");
    dbg.split(['(', ' ', '{'])
        .next()
        .unwrap_or("error")
        .to_string()
}
