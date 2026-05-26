//! OpenTelemetry exporter implementation with multi-tenant attribution.
//!
//! Span hierarchy per request (the SERVER span at HTTP ingress is created by
//! the host's `tower-http` `TraceLayer` and is the parent of `chat` below):
//!
//! ```text
//! chat <inbound-model>          (INTERNAL — full request lifetime)
//! ├── route                     (INTERNAL — routing decision, brief)
//! ├── chat <upstream-model>     (CLIENT   — hop #1, full GenAI attrs)
//! ├── chat <upstream-model>     (CLIENT   — failover hop, etc.)
//! └── settle                    (INTERNAL — settlement summary, brief)
//! ```
//!
//! Span names follow the GenAI semconv: `{gen_ai.operation.name} {gen_ai.request.model}`.
//!
//! W3C `traceparent` propagation:
//! - **Inbound**: extracted from request headers at `PreRequest` (the registered
//!   `TraceContextPropagator` parses any `traceparent`).
//! - **Outbound**: injected into upstream HTTP headers at `on_hop_start` via
//!   `PipelineContext::set_outbound_trace_headers`, picked up by the executor
//!   before the request is sent.
//!
//! Specs:
//! - GenAI semantic conventions: <https://opentelemetry.io/docs/specs/semconv/gen-ai/>
//! - W3C Trace Context: <https://www.w3.org/TR/trace-context/>
//!
//! In-flight spans are tracked in a [`DashMap`] keyed by request id, not a global
//! `Mutex<HashMap>`. The previous draft held a process-wide mutex across every
//! stream part on the hot path.

use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{
    Context, InstrumentationScope, KeyValue, global,
    propagation::{Extractor, Injector},
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
use serde::{Deserialize, Serialize};

use bitrouter_sdk::language_model::{
    ExecutionResult, HopOutcome, ObserveHook, Phase, PipelineContext, Prompt, RequestOutcome,
    RoutingTarget, StreamContext, StreamInterest, StreamPart,
};

use crate::otel::cardinality::CardinalityLimiter;
use crate::otel::config::{OtelConfig, SamplerKind};

/// HTTP header extractor for W3C trace context propagation
/// (<https://www.w3.org/TR/trace-context/>). Used on inbound headers.
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl<'a> Extractor for HeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

/// HTTP header injector for W3C trace context propagation
/// (<https://www.w3.org/TR/trace-context/>). Used to write `traceparent` /
/// `tracestate` into outbound headers before issuing an upstream request.
struct HeaderInjector<'a>(&'a mut http::HeaderMap);

impl<'a> Injector for HeaderInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            http::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

/// In-flight per-request span state. The root `chat` INTERNAL span lives in
/// `context`; the most recent hop's CLIENT span (open across the upstream
/// call) lives in `hop` when present.
struct SpanEntry {
    context: Context,
    created_at: Instant,
    hop: Option<HopState>,
}

/// In-flight per-hop span state — created on `on_hop_start`, consumed on
/// `on_hop_end`. `started_at` is the elapsed-timer source for TTFB
/// propagation to the root chat span on a streaming hop.
struct HopState {
    context: Context,
    started_at: Instant,
}

/// OpenTelemetry exporter with multi-tenant attribution.
pub struct OtelExporter {
    tracer: SdkTracer,
    provider: TracerProvider,
    metrics: Option<crate::otel::metrics::OtelMetrics>,

    /// Snapshot of the config the exporter was built from. Kept so
    /// `status()` can report what's wired without re-reading the YAML.
    config: OtelConfig,

    /// Cardinality limiters — applied to *metric* attributes only. Spans
    /// carry raw values: cardinality is a metrics-storage concern, not a
    /// tracing one, and capping spans loses per-tenant debug fidelity.
    api_key_limiter: Arc<CardinalityLimiter>,
    user_id_limiter: Arc<CardinalityLimiter>,

    /// In-flight spans, keyed by request id.
    active_spans: Arc<DashMap<String, SpanEntry>>,

    /// Maximum time to keep a span before automatic cleanup.
    span_timeout: Duration,

    /// Idempotency guard for [`shutdown`](Self::shutdown). The OTel SDK's
    /// `TracerProvider`/`SdkMeterProvider` panic on a double `shutdown()`,
    /// and the implicit Drop after an explicit shutdown would do exactly
    /// that. `Once` makes the call cheap and safe to invoke from both the
    /// graceful path and any belt-and-braces Drop hook later.
    shutdown_once: Once,
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
        // Versioned instrumentation scope so backends can filter spans by
        // instrumentation library + version. Schema URL pins the semantic
        // conventions snapshot the exporter targets.
        //
        // Spec: https://opentelemetry.io/docs/specs/otel/glossary/#instrumentation-scope
        let scope = InstrumentationScope::builder("io.bitrouter.observe")
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_schema_url(SCHEMA_URL)
            .build();
        let tracer = provider.tracer_with_scope(scope);

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
            config,
            api_key_limiter,
            user_id_limiter,
            active_spans: Arc::new(DashMap::new()),
            span_timeout: Duration::from_secs(300),
            shutdown_once: Once::new(),
        })
    }

    /// Clone the underlying OTel tracer. Used by
    /// [`crate::otel::http_layer::tracing_subscriber_layer`] to build the
    /// `tracing` ↔ OTel bridge. Crate-scoped so the tracer type does not
    /// leak across the plugin boundary.
    pub(crate) fn tracer_clone(&self) -> SdkTracer {
        self.tracer.clone()
    }

    /// Snapshot of what's wired — fed to `bitrouter observe status` via
    /// the daemon control socket. Cheap to call; no allocation beyond the
    /// owned strings.
    pub fn status(&self) -> OtelStatus {
        OtelStatus {
            compiled_in: true,
            exporter_wired: true,
            endpoint: Some(self.config.endpoint.clone()),
            header_count: self.config.headers.len(),
            service_name: Some(self.config.service_name.clone()),
            resource_attribute_count: self.config.resource_attributes.len(),
            sampler: Some(sampler_kind_str(self.config.sampler).to_string()),
            sampler_arg: self.config.sampler_arg,
            metrics_enabled: self.config.metrics.enabled,
            api_key_count: self.api_key_limiter.cardinality(),
            api_key_cap: self.config.metrics.api_key_id_cap,
            user_id_count: self.user_id_limiter.cardinality(),
            user_id_cap: self.config.metrics.user_id_cap,
            active_spans: self.active_spans.len(),
        }
    }

    /// Flush and shut down both the tracer and the metric provider.
    ///
    /// **Synchronous** — must be driven from a context that can park: a
    /// dedicated thread, `tokio::task::spawn_blocking`, or a
    /// non‑async `main`. Calling it directly from an `async fn` parks
    /// the tokio worker that the SDK's `rt-tokio` background tasks need
    /// to make progress, and on a `current_thread` runtime that's a
    /// deadlock. The `OtelObserveHook` adapter in the bin crate wraps
    /// this in `spawn_blocking` for the async path.
    ///
    /// Idempotent: subsequent calls are no‑ops. The SDK itself panics
    /// on double-shutdown; the `Once` guard makes "shutdown then Drop"
    /// safe.
    pub fn shutdown(&self) {
        self.shutdown_once.call_once(|| {
            let _ = self.provider.force_flush();
            let _ = self.provider.shutdown();
            if let Some(m) = &self.metrics {
                m.shutdown();
            }
        });
    }

    fn gc_expired_spans(&self) {
        let now = Instant::now();
        let timeout = self.span_timeout;
        self.active_spans
            .retain(|_, entry| now.duration_since(entry.created_at) < timeout);
    }
}

/// Serializable snapshot of the OTel exporter's state. Returned by
/// [`OtelExporter::status`] and surfaced through the daemon control socket
/// for `bitrouter observe status`. Field names match the YAML / env-var
/// vocabulary so the output reads as "this is what the exporter sees."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelStatus {
    /// Whether the `otel` feature was compiled in. Always `true` on a
    /// snapshot produced by an `OtelExporter`; the daemon may emit a
    /// snapshot with `compiled_in: false` when the feature is off.
    pub compiled_in: bool,
    /// Whether an exporter is actually wired (YAML / env opt-in fired).
    pub exporter_wired: bool,
    /// OTLP/HTTP+protobuf endpoint.
    pub endpoint: Option<String>,
    /// Number of additional headers configured (count only; values not
    /// surfaced to avoid leaking credentials).
    pub header_count: usize,
    /// Service name reported on the resource.
    pub service_name: Option<String>,
    /// Number of `OTEL_RESOURCE_ATTRIBUTES` entries merged in.
    pub resource_attribute_count: usize,
    /// Sampler kind (e.g. `parentbased_always_on`).
    pub sampler: Option<String>,
    /// Sampler ratio argument (only set for `*_traceidratio`).
    pub sampler_arg: Option<f64>,
    /// Whether metrics export is enabled.
    pub metrics_enabled: bool,
    /// Distinct `api_key_id` values currently seen by the cardinality
    /// limiter.
    pub api_key_count: usize,
    /// Cardinality cap for the `api_key_id` metric dimension.
    pub api_key_cap: usize,
    /// Distinct `user_id` values currently seen.
    pub user_id_count: usize,
    /// Cardinality cap for the `user_id` metric dimension.
    pub user_id_cap: usize,
    /// Number of in-flight spans currently tracked.
    pub active_spans: usize,
}

fn sampler_kind_str(s: SamplerKind) -> &'static str {
    match s {
        SamplerKind::AlwaysOn => "always_on",
        SamplerKind::AlwaysOff => "always_off",
        SamplerKind::TraceIdRatio => "traceidratio",
        SamplerKind::ParentBasedAlwaysOn => "parentbased_always_on",
        SamplerKind::ParentBasedAlwaysOff => "parentbased_always_off",
        SamplerKind::ParentBasedTraceIdRatio => "parentbased_traceidratio",
    }
}

/// Transparent newtype around `Arc<OtelExporter>` so the same exporter
/// instance can be registered with the pipeline builder *and* held by
/// the daemon dispatcher for `observe status` queries. Without this, the
/// builder's `observe_hook(impl ObserveHook + 'static)` would move the
/// exporter in, making it unreachable from anywhere else.
///
/// Orphan rules forbid `impl ObserveHook for Arc<OtelExporter>` directly
/// (both types are foreign to this crate); the newtype is the standard
/// workaround.
pub struct OtelObserveHook(Arc<OtelExporter>);

impl OtelObserveHook {
    /// Build a hook handle from a shared exporter.
    pub fn new(exporter: Arc<OtelExporter>) -> Self {
        Self(exporter)
    }
}

#[async_trait]
impl ObserveHook for OtelObserveHook {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        self.0.after_phase(phase, ctx).await
    }

    async fn on_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        self.0.on_hop_start(ctx, target).await
    }

    async fn on_hop_end(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        self.0.on_hop_end(ctx, target, outcome).await
    }

    fn stream_interest(&self) -> StreamInterest {
        self.0.stream_interest()
    }

    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart) {
        self.0.on_stream_part(ctx, part).await
    }

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        self.0.on_request_end(ctx, outcome).await
    }
}

#[async_trait]
impl ObserveHook for OtelExporter {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext) {
        match phase {
            Phase::PreRequest => {
                // Extract inbound W3C trace context, if any. The host's HTTP
                // ingress layer (tower-http TraceLayer + tracing-opentelemetry
                // bridge) creates the SERVER span; we still extract here so
                // the root `chat` INTERNAL span parents correctly when the
                // exporter is used without that ingress layer (e.g. tests).
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
                    .with_kind(SpanKind::Internal)
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
                        hop: None,
                    },
                );
                // Best-effort GC — only walks the map, not held across awaits.
                self.gc_expired_spans();
            }
            Phase::Route => {
                // Brief INTERNAL span recording the routing decision. Parented
                // to the root `chat` span.
                if let Some(entry) = self.active_spans.get(ctx.request_id()) {
                    let mut span = self
                        .tracer
                        .span_builder("route")
                        .with_kind(SpanKind::Internal)
                        .start_with_context(&self.tracer, &entry.context);
                    if let Some(chain) = &ctx.route_chain {
                        span.set_attribute(KeyValue::new(
                            "bitrouter.route_chain_length",
                            chain.len() as i64,
                        ));
                        if let Some(head) = chain.first() {
                            span.set_attribute(KeyValue::new(
                                "bitrouter.route_head_provider",
                                head.provider_name.clone(),
                            ));
                            span.set_attribute(KeyValue::new(
                                "bitrouter.route_head_model",
                                head.service_id.clone(),
                            ));
                        }
                    }
                    span.end();
                }
            }
            Phase::Execution => {
                // Per-hop CLIENT spans cover what the old `bitrouter.execution`
                // span did, with one span per upstream attempt. No work here.
            }
            Phase::Settlement => {
                // Brief INTERNAL span recording the settlement summary.
                // Parented to the root `chat` span.
                if let Some(entry) = self.active_spans.get(ctx.request_id())
                    && let Some(result) = &ctx.execution_result
                {
                    let mut span = self
                        .tracer
                        .span_builder("settle")
                        .with_kind(SpanKind::Internal)
                        .start_with_context(&self.tracer, &entry.context);
                    span.set_attribute(KeyValue::new(
                        "bitrouter.provider_id",
                        result.provider_id.clone(),
                    ));
                    span.set_attribute(KeyValue::new(
                        "bitrouter.model_id",
                        result.model_id.clone(),
                    ));
                    if let Some(label) = &result.account_label {
                        span.set_attribute(KeyValue::new("bitrouter.account_label", label.clone()));
                    }
                    if let Some(usage) = &result.result.usage {
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.input_tokens",
                            usage.prompt_tokens as i64,
                        ));
                        span.set_attribute(KeyValue::new(
                            "gen_ai.usage.output_tokens",
                            usage.completion_tokens as i64,
                        ));
                    }
                    span.end();
                }
            }
        }
    }

    async fn on_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        let Some(mut entry) = self.active_spans.get_mut(ctx.request_id()) else {
            return;
        };

        let span_name = format!("chat {}", target.service_id);
        let attrs = build_hop_request_attrs(target, ctx.prompt());

        let span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .with_attributes(attrs)
            .start_with_context(&self.tracer, &entry.context);
        let hop_context = entry.context.clone().with_span(span);

        // Inject W3C `traceparent` / `tracestate` into a fresh header map and
        // hand it to the SDK; the executor merges it into the upstream HTTP
        // request just before sending. Spec: https://www.w3.org/TR/trace-context/
        let mut headers = http::HeaderMap::new();
        global::get_text_map_propagator(|p| {
            p.inject_context(&hop_context, &mut HeaderInjector(&mut headers));
        });
        ctx.set_outbound_trace_headers(headers);

        entry.hop = Some(HopState {
            context: hop_context,
            started_at: Instant::now(),
        });
    }

    async fn on_hop_end(
        &self,
        ctx: &PipelineContext,
        _target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        // Borrow the map only long enough to take the hop state + clone the
        // root context. Holding the DashMap guard across the span work would
        // serialise unrelated requests sharing a shard.
        let (hop, root_context) = {
            let Some(mut entry) = self.active_spans.get_mut(ctx.request_id()) else {
                return;
            };
            let Some(hop) = entry.hop.take() else {
                return;
            };
            (hop, entry.context.clone())
        };
        let hop_elapsed = hop.started_at.elapsed();
        let is_stream_start = matches!(outcome, HopOutcome::StreamStarted);

        // Close the hop CLIENT span. Access via `Context::span()` instead of
        // attaching the context + `get_active_span` — same effect, fewer
        // moving parts. `SpanRef`'s mutation methods are `&self`.
        let hop_span = hop.context.span();
        match outcome {
            HopOutcome::Generated(result) => {
                set_hop_response_attrs(&hop_span, result);
                hop_span.set_status(Status::Ok);
            }
            HopOutcome::StreamStarted => {
                // Stream handshake reached (TTFB). Body-level attrs land on
                // the root span via `on_request_end`.
                hop_span.set_status(Status::Ok);
            }
            HopOutcome::Failed(err) => {
                let err_class = error_type(err);
                hop_span.set_attribute(KeyValue::new("error.type", err_class.clone()));
                // OTel exception semconv:
                // https://opentelemetry.io/docs/specs/semconv/exceptions/
                hop_span.add_event(
                    "exception",
                    vec![
                        KeyValue::new("exception.type", err_class),
                        KeyValue::new("exception.message", err.to_string()),
                    ],
                );
                hop_span.set_status(Status::error(err.to_string()));
            }
        }
        hop_span.end();

        // For a streaming hop that just reached TTFB, propagate the latency
        // to the root chat span. Spec:
        // gen_ai.response.time_to_first_chunk is in seconds (f64).
        if is_stream_start {
            root_context.span().set_attribute(KeyValue::new(
                "gen_ai.response.time_to_first_chunk",
                hop_elapsed.as_secs_f64(),
            ));
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
            if let StreamPart::ToolCallDelta {
                name: Some(name), ..
            } = part
            {
                opentelemetry::trace::get_active_span(|span| {
                    span.add_event(
                        "tool_call.started",
                        vec![KeyValue::new("tool.name", name.clone())],
                    );
                });
            }
            // Token usage from `StreamPart::Usage` is intentionally NOT
            // written to the span here. The GenAI semconv does not define
            // a per-delta usage attribute, and last-write-wins on a span
            // attribute would be meaningless. Final aggregate usage is
            // recorded once on the terminal `on_request_end`.
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

                // GenAI semconv. `gen_ai.provider.name` is the spec's
                // current key (replaces the older `gen_ai.system`).
                span.set_attribute(KeyValue::new(
                    "gen_ai.provider.name",
                    result.provider_id.clone(),
                ));
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
                    // OTel error.type stays as a low-cardinality attribute
                    // (used by metric dimensions); the human-readable error
                    // message moves into a spec-shaped `exception` event,
                    // alongside any future stack trace.
                    //
                    // Spec: https://opentelemetry.io/docs/specs/semconv/exceptions/
                    span.set_attribute(KeyValue::new("error.type", error_type(err)));
                    span.add_event(
                        "exception",
                        vec![
                            KeyValue::new("exception.type", error_type(err)),
                            KeyValue::new("exception.message", err.to_string()),
                        ],
                    );
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

/// Parse a `RoutingTarget`'s api_base into `(server.address, server.port)`
/// per the GenAI semconv:
/// <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/>.
fn parse_server(api_base: &str) -> Option<(String, Option<u16>)> {
    let uri: http::Uri = api_base.parse().ok()?;
    let host = uri.host()?.to_string();
    let port = uri.port_u16();
    Some((host, port))
}

/// Per-hop request-side attributes. The recommended GenAI attribute set is
/// from the current semantic conventions snapshot
/// (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>); attributes the
/// canonical IR does not carry (e.g. `seed`, `stop_sequences`,
/// `frequency_penalty`, `presence_penalty`) are pulled opportunistically
/// from the prompt's untyped `extra` map under their spec-shaped key names.
fn build_hop_request_attrs(target: &RoutingTarget, prompt: &Prompt) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("gen_ai.operation.name", "chat"),
        KeyValue::new("gen_ai.provider.name", target.provider_name.clone()),
        KeyValue::new("gen_ai.request.model", target.service_id.clone()),
    ];
    if let Some(label) = &target.account_label {
        attrs.push(KeyValue::new("bitrouter.account_label", label.clone()));
    }
    if let Some((host, port)) = parse_server(target.effective_api_base()) {
        attrs.push(KeyValue::new("server.address", host));
        if let Some(port) = port {
            attrs.push(KeyValue::new("server.port", port as i64));
        }
    }

    let params = &prompt.params;
    if let Some(t) = params.temperature {
        attrs.push(KeyValue::new("gen_ai.request.temperature", t));
    }
    if let Some(p) = params.top_p {
        attrs.push(KeyValue::new("gen_ai.request.top_p", p));
    }
    if let Some(m) = params.max_tokens {
        attrs.push(KeyValue::new("gen_ai.request.max_tokens", m as i64));
    }
    // Spec attrs not promoted into the canonical IR — read opportunistically
    // from the untyped extras the prompt carries through.
    if let Some(seed) = params.extra.get("seed").and_then(|v| v.as_i64()) {
        attrs.push(KeyValue::new("gen_ai.request.seed", seed));
    }
    if let Some(fp) = params
        .extra
        .get("frequency_penalty")
        .and_then(|v| v.as_f64())
    {
        attrs.push(KeyValue::new("gen_ai.request.frequency_penalty", fp));
    }
    if let Some(pp) = params
        .extra
        .get("presence_penalty")
        .and_then(|v| v.as_f64())
    {
        attrs.push(KeyValue::new("gen_ai.request.presence_penalty", pp));
    }
    if let Some(stops) = params.extra.get("stop").and_then(|v| v.as_array()) {
        let values: Vec<opentelemetry::StringValue> = stops
            .iter()
            .filter_map(|s| s.as_str().map(|s| s.to_string().into()))
            .collect();
        if !values.is_empty() {
            attrs.push(KeyValue::new(
                "gen_ai.request.stop_sequences",
                opentelemetry::Value::Array(opentelemetry::Array::String(values)),
            ));
        }
    }
    attrs
}

/// Per-hop response-side attributes. Mirrors the request-side recommended
/// set; `gen_ai.response.id` is set when the upstream surfaced one via the
/// canonical IR (today only OpenAI Responses streaming carries a response id
/// natively; non-streaming returns it inside provider-native fields the
/// canonical `GenerateResult` does not yet promote).
fn set_hop_response_attrs(span: &opentelemetry::trace::SpanRef<'_>, result: &ExecutionResult) {
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
    if let Some(reason) = &result.result.finish_reason {
        span.set_attribute(KeyValue::new(
            "gen_ai.response.finish_reasons",
            opentelemetry::Value::Array(opentelemetry::Array::String(vec![
                finish_reason_to_str(reason).into(),
            ])),
        ));
    }
    span.set_attribute(KeyValue::new(
        "bitrouter.latency_ms",
        result.latency_ms as i64,
    ));
    span.set_attribute(KeyValue::new(
        "bitrouter.generation_time_ms",
        result.generation_time_ms as i64,
    ));
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
