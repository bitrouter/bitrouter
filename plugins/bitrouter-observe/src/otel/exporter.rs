//! OpenTelemetry exporter implementation with multi-tenant attribution.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use opentelemetry::{
    global,
    propagation::{Extractor, TextMapPropagator},
    trace::{Span, SpanKind, Status, TraceContextExt, Tracer, TracerProvider},
    Context, KeyValue,
};
use opentelemetry_sdk::{
    trace::{self, RandomIdGenerator, Sampler},
    Resource,
};
use opentelemetry_semantic_conventions::{
    attribute::{SERVICE_NAME, SERVICE_VERSION},
    SCHEMA_URL,
};

use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext, StreamInterest, StreamPart,
};

use crate::otel::{cardinality::CardinalityLimiter, config::OtelConfig, metrics::OtelMetrics};

/// HTTP header extractor for W3C trace context propagation.
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    
    fn keys(&self) -> Vec<&str> {
        self.0.keys().filter_map(|k| k.as_str()).collect()
    }
}

/// Entry for tracking active spans with creation timestamp for cleanup.
struct SpanEntry {
    context: Context,
    created_at: Instant,
}

/// OpenTelemetry exporter with multi-tenant attribution.
pub struct OtelExporter {
    tracer: Box<dyn Tracer + Send + Sync>,
    metrics: Option<OtelMetrics>,
    config: OtelConfig,
    
    // Cardinality limiters for high-cardinality dimensions
    api_key_limiter: Arc<CardinalityLimiter>,
    user_id_limiter: Arc<CardinalityLimiter>,
    
    // Active spans tracked by request_id with timestamps for cleanup
    active_spans: Arc<Mutex<HashMap<String, SpanEntry>>>,
    
    // Maximum time to keep a span before automatic cleanup
    span_timeout: Duration,
}

impl OtelExporter {
    /// Create a new exporter with the given configuration.
    pub fn new(mut config: OtelConfig) -> Result<Self, Box<dyn std::error::Error>> {
        // Apply environment variable overrides
        config = config.with_env_overrides();
        
        // Build resource attributes
        let resource = Resource::from_schema_url(
            [
                KeyValue::new(SERVICE_NAME, "bitrouter"),
                KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")),
            ],
            SCHEMA_URL,
        );
        
        // Create cardinality limiters first (before they're moved)
        let api_key_limiter = Arc::new(CardinalityLimiter::new(config.metrics.api_key_id_cap));
        let user_id_limiter = Arc::new(CardinalityLimiter::new(config.metrics.user_id_cap));
        
        // Configure tracer
        let tracer = match config.protocol {
            crate::otel::config::OtelProtocol::HttpProtobuf => {
                opentelemetry_otlp::new_pipeline()
                    .tracing()
                    .with_exporter(
                        opentelemetry_otlp::new_exporter()
                            .http()
                            .with_endpoint(&config.endpoint)
                            .with_headers(config.headers.clone()),
                    )
                    .with_trace_config(
                        trace::config()
                            .with_sampler(Sampler::AlwaysOn) // MVP: sample everything
                            .with_id_generator(RandomIdGenerator::default())
                            .with_resource(resource.clone()),
                    )
                    .with_batch_config(
                        trace::BatchConfig::default()
                            .with_max_queue_size(config.traces.batch.max_spans)
                            .with_scheduled_delay(std::time::Duration::from_millis(
                                config.traces.batch.flush_ms,
                            )),
                    )
                    .install_batch(opentelemetry_sdk::runtime::Tokio)?
            }
            _ => {
                // TODO: Implement HTTP/JSON and gRPC protocols
                return Err("Only http/protobuf protocol is currently implemented".into());
            }
        };
        
        // Initialize metrics if enabled
        let metrics = if config.metrics.enabled {
            Some(OtelMetrics::new(
                &config,
                resource,
                Arc::clone(&api_key_limiter),
                Arc::clone(&user_id_limiter),
            )?)
        } else {
            None
        };
        
        Ok(Self {
            tracer: tracer.tracer("bitrouter"),
            metrics,
            config,
            api_key_limiter,
            user_id_limiter,
            active_spans: Arc::new(Mutex::new(HashMap::new())),
            span_timeout: Duration::from_secs(300), // 5 minute timeout for abandoned spans
        })
    }
    
    /// Create a no-op exporter for benchmarking (doesn't actually export).
    #[cfg(test)]
    pub fn new_noop() -> Self {
        use opentelemetry::global;
        
        Self {
            tracer: global::tracer("noop"),
            metrics: None,
            config: OtelConfig::default(),
            api_key_limiter: Arc::new(CardinalityLimiter::new(1024)),
            user_id_limiter: Arc::new(CardinalityLimiter::new(256)),
            active_spans: Arc::new(Mutex::new(HashMap::new())),
            span_timeout: Duration::from_secs(300),
        }
    }
    
    /// Get cardinality statistics for monitoring.
    pub fn cardinality_stats(&self) -> CardinalityStats {
        CardinalityStats {
            api_key_count: self.api_key_limiter.cardinality(),
            user_id_count: self.user_id_limiter.cardinality(),
            api_key_cap: self.config.metrics.api_key_id_cap,
            user_id_cap: self.config.metrics.user_id_cap,
        }
    }
}

/// Cardinality statistics for monitoring.
#[derive(Debug, Clone)]
pub struct CardinalityStats {
    pub api_key_count: usize,
    pub user_id_count: usize,
    pub api_key_cap: usize,
    pub user_id_cap: usize,
}

#[async_trait]
impl ObserveHook for OtelExporter {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        match phase {
            Phase::PreRequest => {
                // Create root span at the start of the request
                // Extract W3C trace context from headers if present
                let parent_context = global::get_text_map_propagator(|propagator| {
                    propagator.extract(&HeaderExtractor(ctx.headers()))
                });
                
                // Create root span with parent context if available
                let mut builder = self
                    .tracer
                    .span_builder("bitrouter.request")
                    .with_kind(SpanKind::Server)
                    .with_attributes(vec![
                        KeyValue::new("bitrouter.request_id", ctx.request_id().to_string()),
                        KeyValue::new("bitrouter.model", ctx.model().to_string()),
                        // Multi-tenant attribution from the start
                        KeyValue::new(
                            "bitrouter.api_key_id",
                            self.api_key_limiter.cap(ctx.caller().api_key_id()),
                        ),
                        KeyValue::new(
                            "bitrouter.user_id",
                            self.user_id_limiter.cap(ctx.caller().user_id()),
                        ),
                    ]);
                
                // Set parent context if we extracted one
                let span = if parent_context.span().span_context().is_valid() {
                    builder.start_with_context(&*self.tracer, &parent_context)
                } else {
                    builder.start(&*self.tracer)
                };
                
                // Store span context for child spans with timestamp
                let cx = Context::current_with_span(span);
                let entry = SpanEntry {
                    context: cx,
                    created_at: Instant::now(),
                };
                
                // Clean up old spans while we have the lock
                if let Ok(mut spans) = self.active_spans.lock() {
                    // Remove spans older than timeout
                    let now = Instant::now();
                    spans.retain(|_, entry| now.duration_since(entry.created_at) < self.span_timeout);
                    
                    // Insert new span
                    spans.insert(ctx.request_id().to_string(), entry);
                }
            }
            Phase::Execution => {
                // Create child span for execution
                if let Ok(spans) = self.active_spans.lock() {
                    if let Some(entry) = spans.get(ctx.request_id()) {
                        let _guard = entry.context.clone().attach();
                    
                    let mut span = self
                        .tracer
                        .span_builder("bitrouter.execution")
                        .with_kind(SpanKind::Client)
                        .start(&*self.tracer);
                    
                    // Add routing details if available
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
            }
            _ => {} // Skip other phases for MVP
        }
    }
    
    fn stream_interest(&self) -> StreamInterest {
        // MVP: Don't trace every token, but do count stream events
        StreamInterest::all()
    }
    
    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart) {
        // Count stream parts in metrics if enabled
        if let Some(metrics) = &self.metrics {
            metrics.record_stream_part(part);
        }
        
        // Add important events to the active span
        if let Ok(spans) = self.active_spans.lock() {
            if let Some(entry) = spans.get(ctx.request_id()) {
                let _guard = entry.context.clone().attach();
            
            match part {
                StreamPart::ToolCallDelta { name, .. } if name.is_some() => {
                    // Record tool call start as an event
                    opentelemetry::trace::get_active_span(|span| {
                        span.add_event(
                            "tool_call.started",
                            vec![KeyValue::new("tool.name", name.clone().unwrap())],
                        );
                    });
                }
                StreamPart::Usage { usage } => {
                    // Update span with incremental usage
                    opentelemetry::trace::get_active_span(|span| {
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.stream_tokens",
                            usage.total_tokens() as i64,
                        ));
                    });
                }
                _ => {} // Most stream parts don't need events
            }
            }
        }
    }
    
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        // Complete root span with all details
        let entry = match self.active_spans.lock() {
            Ok(mut spans) => spans.remove(ctx.request_id()),
            Err(poisoned) => {
                tracing::warn!("Active spans mutex poisoned during request end");
                poisoned.into_inner().remove(ctx.request_id())
            }
        };
        
        if let Some(entry) = entry {
            let _guard = entry.context.clone().attach();
            
            opentelemetry::trace::get_active_span(|span| {
                // Add execution results if available
                if let Some(result) = &ctx.execution_result {
                    span.set_attribute(KeyValue::new(
                        "bitrouter.provider_id",
                        result.provider_id.clone(),
                    ));
                    span.set_attribute(KeyValue::new(
                        "bitrouter.model_id",
                        result.model_id.clone(),
                    ));
                    span.set_attribute(KeyValue::new(
                        "bitrouter.latency_ms",
                        result.latency_ms as i64,
                    ));
                    span.set_attribute(KeyValue::new(
                        "bitrouter.generation_time_ms",
                        result.generation_time_ms as i64,
                    ));
                    
                    // Multi-account provider label if present
                    if let Some(label) = &result.account_label {
                        span.set_attribute(KeyValue::new("bitrouter.account_label", label.clone()));
                    }
                    
                    // GenAI semantic conventions
                    span.set_attribute(KeyValue::new("gen_ai.system", result.provider_id.clone()));
                    span.set_attribute(KeyValue::new("gen_ai.request.model", ctx.model().to_string()));
                    span.set_attribute(KeyValue::new(
                        "gen_ai.response.model",
                        result.model_id.clone(),
                    ));
                    
                    // Token usage if reported
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
                    
                    // Finish reason if present
                    if let Some(reason) = &result.result.finish_reason {
                        span.set_attribute(KeyValue::new(
                            "gen_ai.response.finish_reason",
                            format!("{:?}", reason),
                        ));
                    }
                }
                
                // Set span status based on outcome
                match outcome {
                    RequestOutcome::Completed => {
                        span.set_status(Status::Ok);
                        span.set_attribute(KeyValue::new("bitrouter.outcome", "completed"));
                    }
                    RequestOutcome::Failed(err) => {
                        span.set_status(Status::error(err.to_string()));
                        span.set_attribute(KeyValue::new("bitrouter.outcome", "failed"));
                        span.set_attribute(KeyValue::new("error.message", err.to_string()));
                    }
                    RequestOutcome::ClientDisconnected => {
                        span.set_status(Status::error("client_disconnected"));
                        span.set_attribute(KeyValue::new("bitrouter.outcome", "disconnected"));
                    }
                }
                
                span.end();
            });
        }
        
        // Record metrics if enabled
        if let Some(metrics) = &self.metrics {
            metrics.record_request(ctx, outcome);
        }
    }
}

// Note: We don't implement Drop to call global::shutdown_tracer_provider()
// because that affects ALL tracer instances globally, not just this one.
// Proper shutdown should be handled at application level.