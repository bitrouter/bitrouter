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
use crate::otel::config::{ContentCaptureMode, OtelConfig, SamplerKind};
use crate::otel::span_attributes::SpanAttributes;

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

        // Transport (OTLP/HTTP vs OTLP/gRPC) is chosen at compile time by the
        // crate's feature flags — see `crate::otel::transport`.
        let span_exporter = crate::otel::transport::span_exporter(&config)?;

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

    /// Clone the underlying OTel SDK tracer. Used by
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
                // Pick a parent for the root `chat` INTERNAL span:
                //   1. If the host's `tower-http` `TraceLayer` +
                //      `tracing-opentelemetry` bridge wrapped this request in
                //      a SERVER span, parent on it. The bridge does NOT
                //      synchronise `opentelemetry::Context::current()` with
                //      tracing's current span across async awaits — it stores
                //      the OTel data in the tracing-span extensions instead.
                //      `tracing::Span::current().context()` does the lookup.
                //   2. Otherwise (no ingress layer — typical in unit tests),
                //      fall back to extracting an inbound `traceparent` from
                //      the request headers via the W3C propagator. Spec:
                //      <https://www.w3.org/TR/trace-context/>
                use tracing_opentelemetry::OpenTelemetrySpanExt as _;
                let bridge_cx = tracing::Span::current().context();
                let parent_context = if bridge_cx.span().span_context().is_valid() {
                    bridge_cx
                } else {
                    global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(ctx.headers())))
                };

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
            match part {
                StreamPart::ToolCallDelta {
                    name: Some(name), ..
                } => {
                    let _guard = entry.context.clone().attach();
                    opentelemetry::trace::get_active_span(|span| {
                        span.add_event(
                            "tool_call.started",
                            vec![KeyValue::new("tool.name", name.clone())],
                        );
                    });
                }
                StreamPart::ResponseStarted { id } | StreamPart::ResponseCompleted { id, .. } => {
                    // The upstream response id, surfaced by the decoder near
                    // the start of the stream (`ResponseStarted`, emitted by
                    // Chat Completions / Messages / Generate Content) or on the terminal
                    // frame (`ResponseCompleted`, Responses). Stamp it
                    // onto the root `chat` span as `gen_ai.response.id`; the
                    // non-streaming path does the same via
                    // `GenerateResult.response_id`. Spec:
                    // https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/
                    entry
                        .context
                        .span()
                        .set_attribute(KeyValue::new("gen_ai.response.id", id.clone()));
                }
                _ => {
                    // Token usage from `StreamPart::Usage` is intentionally
                    // NOT written to the span here. The GenAI semconv does
                    // not define a per-delta usage attribute, and
                    // last-write-wins on a span attribute would be
                    // meaningless. Final aggregate usage is recorded once on
                    // the terminal `on_request_end`.
                }
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
                if let Some(id) = &result.result.response_id {
                    span.set_attribute(KeyValue::new("gen_ai.response.id", id.clone()));
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

            // Cloud-forwarded attributes (cost / namespace / routing / …). A
            // `SettlementRecorder` may emit `SpanAttributes`;
            // `PipelineContext::absorb_settlement` folds the settlement bus
            // back into `ctx` before this hook runs, so they're visible here.
            // Generic by design — the emitter names the keys, we just stamp.
            if let Some(extra) = ctx.get_event::<SpanAttributes>() {
                for (key, value) in &extra.0 {
                    match value {
                        serde_json::Value::String(s) => {
                            span.set_attribute(KeyValue::new(key.clone(), s.clone()));
                        }
                        serde_json::Value::Bool(b) => {
                            span.set_attribute(KeyValue::new(key.clone(), *b));
                        }
                        serde_json::Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                span.set_attribute(KeyValue::new(key.clone(), i));
                            } else if let Some(f) = n.as_f64() {
                                span.set_attribute(KeyValue::new(key.clone(), f));
                            }
                        }
                        // null / array / object: not a scalar OTel value — skip.
                        _ => {}
                    }
                }
            }

            // Optional prompt / response content capture (off by default).
            if self.config.content_capture == ContentCaptureMode::Full {
                if let Ok(json) = serde_json::to_string(&ctx.prompt().messages) {
                    span.set_attribute(KeyValue::new(
                        "gen_ai.input.messages",
                        truncate_utf8(json, CONTENT_ATTR_MAX_BYTES),
                    ));
                }
                if let Some(result) = &ctx.execution_result
                    && let Ok(json) = serde_json::to_string(&result.result.content)
                {
                    span.set_attribute(KeyValue::new(
                        "gen_ai.output.messages",
                        truncate_utf8(json, CONTENT_ATTR_MAX_BYTES),
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

/// Maximum byte length for a single captured content attribute
/// (`gen_ai.input.messages` / `gen_ai.output.messages`). A conservative cap so
/// a pathological prompt or response can't produce an oversized span the
/// collector or backend would reject. Only consulted under
/// [`ContentCaptureMode::Full`].
const CONTENT_ATTR_MAX_BYTES: usize = 128 * 1024;

/// Truncate a `String` to at most `max` bytes, backing off to the nearest
/// UTF-8 char boundary so the result is always valid UTF-8.
fn truncate_utf8(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
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
/// set; `gen_ai.response.id` is read from the canonical IR's
/// `GenerateResult.response_id`, which the outbound adapters populate
/// from the provider-native id field (OpenAI `chatcmpl-...`, Anthropic
/// `msg_...`, Responses `resp_...`, Google `responseId`).
///
/// Streaming hops do not pass through here — they close at TTFB via
/// `HopOutcome::StreamStarted` without an `ExecutionResult`. The
/// streaming response id (when surfaced — currently only OpenAI
/// Responses' `StreamPart::ResponseCompleted`) lands on the root chat
/// span via `on_stream_part`, not on the hop CLIENT span.
fn set_hop_response_attrs(span: &opentelemetry::trace::SpanRef<'_>, result: &ExecutionResult) {
    span.set_attribute(KeyValue::new(
        "gen_ai.response.model",
        result.model_id.clone(),
    ));
    if let Some(id) = &result.result.response_id {
        span.set_attribute(KeyValue::new("gen_ai.response.id", id.clone()));
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

#[cfg(test)]
mod hop_tests {
    //! Unit tests for the per-hop `ObserveHook` surface added by issue #477.
    //! These drive the trait methods against an in-process span processor
    //! that captures every exported [`SpanData`], so the assertions look at
    //! span structure directly instead of decoding OTLP wire bytes.

    use super::*;

    use std::sync::Mutex;

    use opentelemetry::Value;
    use opentelemetry::trace::TraceResult;
    use opentelemetry_sdk::export::trace::SpanData;
    use opentelemetry_sdk::trace::{Span as SdkSpan, SpanProcessor};

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::error::BitrouterError;
    use bitrouter_sdk::language_model::{
        ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message,
        PipelineRequest, Prompt, Role, Usage,
    };

    /// In-process span processor that appends every ended span to a shared
    /// vector. Replaces the OTLP/HTTP exporter for tests so assertions can
    /// inspect span structure (names, kinds, parents, attributes, events)
    /// without doing protobuf decoding.
    #[derive(Clone, Debug)]
    struct CapturingProcessor {
        captured: Arc<Mutex<Vec<SpanData>>>,
    }

    impl SpanProcessor for CapturingProcessor {
        fn on_start(&self, _span: &mut SdkSpan, _cx: &Context) {}

        fn on_end(&self, span: SpanData) {
            let mut guard = match self.captured.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.push(span);
        }

        fn force_flush(&self) -> TraceResult<()> {
            Ok(())
        }

        fn shutdown(&self) -> TraceResult<()> {
            Ok(())
        }
    }

    /// Build an [`OtelExporter`] backed by an in-process span processor.
    /// Bypasses [`OtelExporter::new`] (which insists on an OTLP HTTP
    /// endpoint) by constructing the struct directly — possible here
    /// because the test lives in the same module and sees private fields.
    fn make_test_exporter() -> (OtelExporter, Arc<Mutex<Vec<SpanData>>>) {
        make_test_exporter_with(OtelConfig::default())
    }

    fn make_test_exporter_with(config: OtelConfig) -> (OtelExporter, Arc<Mutex<Vec<SpanData>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let processor = CapturingProcessor {
            captured: captured.clone(),
        };
        global::set_text_map_propagator(TraceContextPropagator::new());

        let provider = TracerProvider::builder()
            .with_span_processor(processor)
            .with_sampler(Sampler::AlwaysOn)
            .with_id_generator(RandomIdGenerator::default())
            .build();
        let scope = InstrumentationScope::builder("io.bitrouter.observe.test").build();
        let tracer = provider.tracer_with_scope(scope);

        let exporter = OtelExporter {
            tracer,
            provider,
            metrics: None,
            config,
            api_key_limiter: Arc::new(CardinalityLimiter::new(100)),
            user_id_limiter: Arc::new(CardinalityLimiter::new(100)),
            active_spans: Arc::new(DashMap::new()),
            span_timeout: Duration::from_secs(300),
            shutdown_once: Once::new(),
        };
        (exporter, captured)
    }

    fn fresh_target(provider: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: provider.to_string(),
            service_id: "test-model".to_string(),
            api_base: "https://api.example.test:8443/v1".to_string(),
            api_key: "k".to_string(),
            api_protocol: ApiProtocol::ChatCompletions,
            account_label: Some("primary".to_string()),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    fn fresh_request() -> PipelineRequest {
        let params = GenerationParams {
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(256),
            ..Default::default()
        };
        let prompt = Prompt {
            model: "test-model".to_string(),
            system: None,
            messages: vec![Message::text(Role::User, "hi")],
            tools: Vec::new(),
            params,
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        PipelineRequest::new("test-model", CallerContext::new("k1", "u1"), prompt)
    }

    fn fresh_result(target: &RoutingTarget) -> ExecutionResult {
        ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: GenerateResult {
                content: vec![Content::Text { text: "ok".into() }],
                usage: Some(Usage {
                    prompt_tokens: 11,
                    completion_tokens: 7,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
                response_id: Some("chatcmpl-test123".into()),
                stop_details: None,
            },
            latency_ms: 42,
            generation_time_ms: 40,
        }
    }

    /// Look up a string-valued attribute on a captured SpanData.
    fn str_attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a str> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match &kv.value {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
    }

    fn i64_attr(span: &SpanData, key: &str) -> Option<i64> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match kv.value {
                Value::I64(v) => Some(v),
                _ => None,
            })
    }

    fn f64_attr(span: &SpanData, key: &str) -> Option<f64> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match kv.value {
                Value::F64(v) => Some(v),
                _ => None,
            })
    }

    fn bool_attr(span: &SpanData, key: &str) -> Option<bool> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match kv.value {
                Value::Bool(v) => Some(v),
                _ => None,
            })
    }

    #[tokio::test]
    async fn on_hop_start_populates_outbound_trace_headers() {
        // `ObserveHook::on_hop_start` must write a W3C-shaped header map onto
        // `PipelineContext::set_outbound_trace_headers` so `HttpExecutor`
        // can merge it into the upstream HTTP request.
        let (exporter, _captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;

        let headers = ctx
            .take_outbound_trace_headers()
            .expect("on_hop_start must stash outbound headers");
        let tp = headers
            .get("traceparent")
            .expect("traceparent header injected")
            .to_str()
            .expect("traceparent is ASCII");
        // Format: 00-<32-hex trace_id>-<16-hex span_id>-<flags>.
        // https://www.w3.org/TR/trace-context/
        assert!(
            tp.starts_with("00-") && tp.len() == 55,
            "traceparent must be a valid W3C v0 string; got {tp}"
        );
    }

    #[tokio::test]
    async fn on_hop_end_writes_genai_attrs_and_parents_on_root_chat() {
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        let result = fresh_result(&target);
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::Generated(&result))
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");
        let hop_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Client)
            .expect("per-hop chat CLIENT span");

        // Hop parents on root chat.
        assert_eq!(
            hop_chat.parent_span_id,
            root_chat.span_context.span_id(),
            "per-hop CLIENT span parents on the root chat INTERNAL span"
        );
        // Same trace.
        assert_eq!(
            hop_chat.span_context.trace_id(),
            root_chat.span_context.trace_id()
        );

        // Required GenAI request-side attributes:
        // https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/
        assert_eq!(str_attr(hop_chat, "gen_ai.operation.name"), Some("chat"));
        assert_eq!(str_attr(hop_chat, "gen_ai.provider.name"), Some("openai"));
        assert_eq!(
            str_attr(hop_chat, "gen_ai.request.model"),
            Some("test-model")
        );
        assert_eq!(
            str_attr(hop_chat, "server.address"),
            Some("api.example.test")
        );
        assert_eq!(i64_attr(hop_chat, "server.port"), Some(8443));
        assert_eq!(
            str_attr(hop_chat, "bitrouter.account_label"),
            Some("primary")
        );

        // Response-side attributes populated on success.
        assert_eq!(
            str_attr(hop_chat, "gen_ai.response.model"),
            Some("test-model")
        );
        // `gen_ai.response.id` mirrors the canonical IR's
        // `GenerateResult.response_id`, which the adapters extract from the
        // provider-native id field.
        assert_eq!(
            str_attr(hop_chat, "gen_ai.response.id"),
            Some("chatcmpl-test123")
        );
        assert_eq!(i64_attr(hop_chat, "gen_ai.usage.input_tokens"), Some(11));
        assert_eq!(i64_attr(hop_chat, "gen_ai.usage.output_tokens"), Some(7));
    }

    #[tokio::test]
    async fn on_request_end_stamps_forwarded_span_attributes() {
        // A SettlementRecorder forwards cloud-computed attributes via a
        // `SpanAttributes` event (#529); `absorb_settlement` makes them visible
        // on the PipelineContext, and `on_request_end` stamps each entry — by
        // JSON type — onto the root `chat` span.
        let (exporter, captured) = make_test_exporter();
        let mut ctx = PipelineContext::new(fresh_request());

        exporter.after_phase(Phase::PreRequest, &ctx).await;

        let mut attrs = serde_json::Map::new();
        attrs.insert("$ai_total_cost_usd".into(), serde_json::json!(0.00123456));
        attrs.insert("namespace".into(), serde_json::json!("acme"));
        attrs.insert("fallback".into(), serde_json::json!(true));
        attrs.insert("bitrouter.retry_count".into(), serde_json::json!(2));
        // Null / nested values are skipped (not representable as a scalar attr).
        attrs.insert("skipped_null".into(), serde_json::Value::Null);
        ctx.emit(SpanAttributes(attrs));

        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");

        assert_eq!(str_attr(root_chat, "namespace"), Some("acme"));
        assert_eq!(f64_attr(root_chat, "$ai_total_cost_usd"), Some(0.00123456));
        assert_eq!(bool_attr(root_chat, "fallback"), Some(true));
        assert_eq!(i64_attr(root_chat, "bitrouter.retry_count"), Some(2));
        assert!(
            root_chat
                .attributes
                .iter()
                .all(|kv| kv.key.as_str() != "skipped_null"),
            "null values are not stamped onto the span"
        );
    }

    #[tokio::test]
    async fn content_capture_off_omits_messages_full_records_them() {
        let target = fresh_target("openai");

        // Off (default): no message bodies on the span.
        let (exporter, captured) = make_test_exporter();
        let mut ctx = PipelineContext::new(fresh_request());
        ctx.execution_result = Some(fresh_result(&target));
        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();
        let spans = captured.lock().unwrap().clone();
        let root = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");
        assert_eq!(str_attr(root, "gen_ai.input.messages"), None);
        assert_eq!(str_attr(root, "gen_ai.output.messages"), None);

        // Full: prompt + response content serialized onto the span.
        let cfg = OtelConfig {
            content_capture: ContentCaptureMode::Full,
            ..OtelConfig::default()
        };
        let (exporter, captured) = make_test_exporter_with(cfg);
        let mut ctx = PipelineContext::new(fresh_request());
        ctx.execution_result = Some(fresh_result(&target));
        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();
        let spans = captured.lock().unwrap().clone();
        let root = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");
        let input = str_attr(root, "gen_ai.input.messages").expect("input messages captured");
        assert!(input.contains("hi"), "prompt text serialized: {input}");
        let output = str_attr(root, "gen_ai.output.messages").expect("output messages captured");
        assert!(output.contains("ok"), "response text serialized: {output}");
    }

    #[tokio::test]
    async fn route_and_settle_internal_spans_parent_on_root_chat() {
        // The exporter emits brief `route` and `settle` INTERNAL spans at
        // their respective phase boundaries; they should sit alongside the
        // hop CLIENT span as children of the root `chat` INTERNAL span.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        // `route` opens + closes synchronously at the Route phase boundary.
        // The exporter reads `ctx.route_chain`, so populate it first.
        let mut ctx = ctx;
        ctx.route_chain = Some(vec![target.clone()]);
        let result = fresh_result(&target);
        ctx.execution_result = Some(result.clone());
        exporter.after_phase(Phase::Route, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::Generated(&result))
            .await;
        exporter.after_phase(Phase::Settlement, &ctx).await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat span");
        let route = spans
            .iter()
            .find(|s| s.name == "route")
            .expect("route INTERNAL span present");
        let settle = spans
            .iter()
            .find(|s| s.name == "settle")
            .expect("settle INTERNAL span present");

        assert_eq!(route.span_kind, SpanKind::Internal);
        assert_eq!(settle.span_kind, SpanKind::Internal);
        assert_eq!(
            route.parent_span_id,
            root_chat.span_context.span_id(),
            "`route` parents on root chat"
        );
        assert_eq!(
            settle.parent_span_id,
            root_chat.span_context.span_id(),
            "`settle` parents on root chat"
        );
        assert_eq!(
            route.span_context.trace_id(),
            root_chat.span_context.trace_id()
        );
        assert_eq!(
            settle.span_context.trace_id(),
            root_chat.span_context.trace_id()
        );
        // route carries the routing summary.
        assert_eq!(i64_attr(route, "bitrouter.route_chain_length"), Some(1));
        assert_eq!(
            str_attr(route, "bitrouter.route_head_provider"),
            Some("openai")
        );
        // settle carries the per-request usage and provider.
        assert_eq!(str_attr(settle, "bitrouter.provider_id"), Some("openai"));
        assert_eq!(i64_attr(settle, "gen_ai.usage.input_tokens"), Some(11));
        assert_eq!(i64_attr(settle, "gen_ai.usage.output_tokens"), Some(7));
    }

    #[tokio::test]
    async fn streaming_hop_propagates_ttfb_to_root_chat() {
        // `HopOutcome::StreamStarted` is the TTFB moment for a streaming
        // request. The exporter writes `gen_ai.response.time_to_first_chunk`
        // (seconds) on the root chat INTERNAL span, not the hop CLIENT span.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        // Brief sleep so the elapsed time TTFB is > 0 — the attribute is
        // an `f64` of seconds; even a few milliseconds is enough to make
        // a non-zero assertion meaningful.
        tokio::time::sleep(Duration::from_millis(5)).await;
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::StreamStarted)
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat span");
        let hop_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Client)
            .expect("per-hop chat CLIENT span");

        // TTFB lives on the ROOT chat span, not on the hop.
        let ttfb = root_chat
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "gen_ai.response.time_to_first_chunk")
            .and_then(|kv| match kv.value {
                Value::F64(v) => Some(v),
                _ => None,
            })
            .expect("root chat carries gen_ai.response.time_to_first_chunk on stream start");
        assert!(
            ttfb > 0.0,
            "time_to_first_chunk should be positive seconds; got {ttfb}"
        );
        assert!(
            hop_chat
                .attributes
                .iter()
                .all(|kv| kv.key.as_str() != "gen_ai.response.time_to_first_chunk"),
            "TTFB belongs on the root chat span, not the hop"
        );
    }

    #[tokio::test]
    async fn multi_hop_failover_emits_one_client_span_per_attempt() {
        // The fallback loop fires `on_hop_start` / `on_hop_end` once per
        // upstream attempt. A 2-hop chain where the first fails and the
        // second succeeds must emit two sibling CLIENT spans, both
        // parented on the same root chat INTERNAL span — that's the
        // exact shape the original issue #477 calls out as the most
        // common multi-account-failover debugging scenario.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let first = fresh_target("primary");
        let second = fresh_target("backup");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        // Hop 1: fails.
        exporter.on_hop_start(&ctx, &first).await;
        let err = BitrouterError::Upstream {
            status: 503,
            message: "down".into(),
        };
        exporter
            .on_hop_end(&ctx, &first, HopOutcome::Failed(&err))
            .await;
        // Hop 2: succeeds.
        exporter.on_hop_start(&ctx, &second).await;
        let result = fresh_result(&second);
        exporter
            .on_hop_end(&ctx, &second, HopOutcome::Generated(&result))
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat span");
        let hop_spans: Vec<&SpanData> = spans
            .iter()
            .filter(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Client)
            .collect();
        assert_eq!(
            hop_spans.len(),
            2,
            "one CLIENT span per upstream attempt — got {}",
            hop_spans.len()
        );
        for hop in &hop_spans {
            assert_eq!(
                hop.parent_span_id,
                root_chat.span_context.span_id(),
                "every hop CLIENT span parents on the root chat INTERNAL span"
            );
            assert_eq!(
                hop.span_context.trace_id(),
                root_chat.span_context.trace_id(),
                "every hop CLIENT span lives in the same trace as the root"
            );
        }

        // The first hop carries the exception event; the second does not.
        let with_exception: Vec<&SpanData> = hop_spans
            .iter()
            .filter(|s| s.events.iter().any(|e| e.name == "exception"))
            .copied()
            .collect();
        assert_eq!(
            with_exception.len(),
            1,
            "only the failed hop carries an `exception` event"
        );
    }

    #[tokio::test]
    async fn stream_response_completed_lands_response_id_on_root_chat() {
        // Responses' terminal `response.completed` frame surfaces
        // as `StreamPart::ResponseCompleted { id, .. }`. The exporter must
        // stamp it onto the root `chat` INTERNAL span as
        // `gen_ai.response.id` so streaming requests aren't missing the
        // per-spec attribute that operators correlate against.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::StreamStarted)
            .await;
        // Feed the terminal stream frame in.
        let stream_ctx = ctx.stream_context();
        exporter
            .on_stream_part(
                &stream_ctx,
                &StreamPart::ResponseCompleted {
                    id: "resp_streamed_xyz".to_string(),
                    status: "completed".to_string(),
                    usage: None,
                },
            )
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");
        assert_eq!(
            str_attr(root_chat, "gen_ai.response.id"),
            Some("resp_streamed_xyz")
        );
    }

    #[tokio::test]
    async fn stream_response_started_lands_response_id_on_root_chat() {
        // Chat Completions / Messages / Generate Content streaming surface the upstream id
        // early as `StreamPart::ResponseStarted { id }`; the exporter stamps
        // it onto the root `chat` INTERNAL span as `gen_ai.response.id`, the
        // same attribute the non-streaming path and `ResponseCompleted` set.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::StreamStarted)
            .await;
        let stream_ctx = ctx.stream_context();
        exporter
            .on_stream_part(
                &stream_ctx,
                &StreamPart::ResponseStarted {
                    id: "chatcmpl-streamed".to_string(),
                },
            )
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Completed)
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let root_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Internal)
            .expect("root chat INTERNAL span");
        assert_eq!(
            str_attr(root_chat, "gen_ai.response.id"),
            Some("chatcmpl-streamed")
        );
    }

    #[tokio::test]
    async fn on_hop_end_records_exception_event_on_failure() {
        // Per OTel exceptions semconv
        // (https://opentelemetry.io/docs/specs/semconv/exceptions/), a
        // failing hop attaches an `exception` event carrying
        // `exception.type` / `exception.message` plus an `error.type`
        // attribute for low-cardinality metric dimensions.
        let (exporter, captured) = make_test_exporter();
        let ctx = PipelineContext::new(fresh_request());
        let target = fresh_target("openai");

        exporter.after_phase(Phase::PreRequest, &ctx).await;
        exporter.on_hop_start(&ctx, &target).await;
        let err = BitrouterError::Upstream {
            status: 503,
            message: "upstream down".into(),
        };
        exporter
            .on_hop_end(&ctx, &target, HopOutcome::Failed(&err))
            .await;
        exporter
            .on_request_end(&ctx, &RequestOutcome::Failed(err.clone()))
            .await;
        exporter.provider.force_flush();

        let spans = captured.lock().unwrap().clone();
        let hop_chat = spans
            .iter()
            .find(|s| s.name == "chat test-model" && s.span_kind == SpanKind::Client)
            .expect("per-hop chat CLIENT span");

        assert!(
            str_attr(hop_chat, "error.type").is_some(),
            "failed hop must carry low-cardinality `error.type`"
        );
        let exception_event = hop_chat
            .events
            .iter()
            .find(|e| e.name == "exception")
            .expect("hop carries an `exception` event on failure");
        let event_attr = |key: &str| {
            exception_event
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .and_then(|kv| match &kv.value {
                    Value::String(s) => Some(s.as_str().to_string()),
                    _ => None,
                })
        };
        assert!(event_attr("exception.type").is_some());
        assert!(
            event_attr("exception.message")
                .map(|m| m.contains("upstream down") || m.contains("Upstream"))
                .unwrap_or(false),
            "exception.message should carry the upstream error text"
        );
    }
}
