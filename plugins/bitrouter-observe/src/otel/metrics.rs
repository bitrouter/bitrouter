//! OpenTelemetry metrics with multi-tenant dimensions.

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::{
    global,
    metrics::{Counter, Histogram, Meter, MeterProvider},
    KeyValue,
};
use opentelemetry_sdk::{
    metrics::{self, Aggregation, PeriodicReader, SdkMeterProvider},
    Resource,
};

use bitrouter_sdk::language_model::{
    PipelineContext, RequestOutcome, StreamPart,
};

use crate::otel::{cardinality::CardinalityLimiter, config::OtelConfig};

/// OpenTelemetry metrics with multi-tenant attribution.
pub struct OtelMetrics {
    meter: Meter,
    
    // Core metrics
    request_counter: Counter<u64>,
    latency_histogram: Histogram<f64>,
    token_counter: Counter<u64>,
    error_counter: Counter<u64>,
    stream_parts_counter: Counter<u64>,
    
    // Observability health metrics
    spans_dropped: Counter<u64>,
    metrics_dropped: Counter<u64>,
    
    // Cardinality limiters (shared with tracer)
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
        // Configure metrics exporter
        let exporter = match config.protocol {
            crate::otel::config::OtelProtocol::HttpProtobuf => {
                opentelemetry_otlp::new_exporter()
                    .http()
                    .with_endpoint(&config.endpoint)
                    .with_headers(config.headers.clone())
                    .build_metrics_exporter(
                        Box::new(Aggregation::default()),
                        Box::new(opentelemetry_sdk::runtime::Tokio),
                    )?
            }
            _ => {
                return Err("Only http/protobuf protocol is currently implemented for metrics".into());
            }
        };
        
        // Create periodic reader
        let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_interval(Duration::from_millis(config.metrics.export_interval_ms))
            .build();
        
        // Build meter provider
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();
        
        // Set as global default
        global::set_meter_provider(provider.clone());
        
        let meter = provider.meter("bitrouter");
        
        // Initialize metrics
        let request_counter = meter
            .u64_counter("bitrouter.requests")
            .with_description("Total requests processed")
            .with_unit("1")
            .init();
        
        let latency_histogram = meter
            .f64_histogram("bitrouter.request.latency")
            .with_description("Request latency in milliseconds")
            .with_unit("ms")
            .init();
        
        let token_counter = meter
            .u64_counter("bitrouter.tokens")
            .with_description("Total tokens processed")
            .with_unit("token")
            .init();
        
        let error_counter = meter
            .u64_counter("bitrouter.errors")
            .with_description("Total errors encountered")
            .with_unit("1")
            .init();
        
        let stream_parts_counter = meter
            .u64_counter("bitrouter.stream_parts")
            .with_description("Total stream parts processed")
            .with_unit("1")
            .init();
        
        let spans_dropped = meter
            .u64_counter("bitrouter.otel.spans_dropped")
            .with_description("Spans dropped due to buffer overflow")
            .with_unit("1")
            .init();
        
        let metrics_dropped = meter
            .u64_counter("bitrouter.otel.metrics_dropped")
            .with_description("Metrics dropped due to buffer overflow")
            .with_unit("1")
            .init();
        
        Ok(Self {
            meter,
            request_counter,
            latency_histogram,
            token_counter,
            error_counter,
            stream_parts_counter,
            spans_dropped,
            metrics_dropped,
            api_key_limiter,
            user_id_limiter,
        })
    }
    
    /// Record a completed request.
    pub fn record_request(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        let mut attributes = self.build_base_attributes(ctx);
        
        // Add outcome
        let outcome_str = match outcome {
            RequestOutcome::Completed => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::ClientDisconnected => "disconnected",
        };
        attributes.push(KeyValue::new("outcome", outcome_str));
        
        // Add execution details if available
        if let Some(result) = &ctx.execution_result {
            attributes.push(KeyValue::new("provider_id", result.provider_id.clone()));
            attributes.push(KeyValue::new("model", result.model_id.clone()));
            
            if let Some(label) = &result.account_label {
                attributes.push(KeyValue::new("account_label", label.clone()));
            }
            
            // Record latency
            self.latency_histogram
                .record(result.latency_ms as f64, &attributes);
            
            // Record token usage
            if let Some(usage) = &result.result.usage {
                self.token_counter
                    .add(usage.total_tokens() as u64, &attributes);
            }
        }
        
        // Increment request counter
        self.request_counter.add(1, &attributes);
        
        // Record errors
        if matches!(outcome, RequestOutcome::Failed(_)) {
            self.error_counter.add(1, &attributes);
        }
    }
    
    /// Record a stream part.
    pub fn record_stream_part(&self, part: &StreamPart) {
        // Simple counter for now - could add more granular metrics later
        self.stream_parts_counter.add(
            1,
            &[KeyValue::new("part_type", stream_part_type(part))],
        );
    }
    
    /// Record dropped spans (called by exporter on buffer overflow).
    pub fn record_spans_dropped(&self, count: u64, reason: &str) {
        self.spans_dropped
            .add(count, &[KeyValue::new("reason", reason.to_string())]);
    }
    
    /// Record dropped metrics (called on buffer overflow).
    pub fn record_metrics_dropped(&self, count: u64, reason: &str) {
        self.metrics_dropped
            .add(count, &[KeyValue::new("reason", reason.to_string())]);
    }
    
    /// Build base attributes with tenant information.
    fn build_base_attributes(&self, ctx: &PipelineContext) -> Vec<KeyValue> {
        let caller = ctx.caller();
        vec![
            KeyValue::new("api_key_id", self.api_key_limiter.cap(caller.api_key_id())),
            KeyValue::new("user_id", self.user_id_limiter.cap(caller.user_id())),
        ]
    }
}

/// Get the type name for a stream part.
fn stream_part_type(part: &StreamPart) -> &'static str {
    match part {
        StreamPart::TextDelta { .. } => "text_delta",
        StreamPart::ReasoningDelta { .. } => "reasoning_delta",
        StreamPart::ToolCallDelta { .. } => "tool_call_delta",
        StreamPart::Usage { .. } => "usage",
        StreamPart::Finish { .. } => "finish",
        StreamPart::ResponseCompleted { .. } => "response_completed",
    }
}

impl Drop for OtelMetrics {
    fn drop(&mut self) {
        // Ensure remaining metrics are exported
        global::shutdown_meter_provider();
    }
}