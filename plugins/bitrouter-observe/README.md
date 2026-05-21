# bitrouter-observe

Multi-tenant observability plugin for BitRouter providing OpenTelemetry traces and metrics with tenant attribution.

## Features

- **OpenTelemetry Integration**: Full OTLP export for traces and metrics
- **Multi-Tenant Attribution**: Every span and metric includes `api_key_id` and `user_id` 
- **Multi-Account Support**: Tracks which provider account served each request
- **W3C Trace Context**: Propagates distributed trace context via `traceparent` header
- **Cardinality Management**: Automatic capping of high-cardinality dimensions
- **GenAI Semantic Conventions**: Follows OpenTelemetry semantic conventions for LLM observability
- **MVP Span Design**: Simple two-span model (request + execution) for clarity
- **Performance Benchmarks**: Measure observability overhead with included benchmarks

## Breaking Changes from v0

- **Prometheus Removed**: The `/metrics` endpoint has been completely removed in favor of OTLP push
- **Feature Flag Renamed**: `otlp` → `otel` 
- **Configuration Changed**: New nested structure under `plugins.bitrouter-observe.otel`

## Configuration

### Basic Setup

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "https://api.honeycomb.io"
      headers:
        x-honeycomb-team: "${HONEYCOMB_API_KEY}"
      service_name: "bitrouter"
      sampler: parentbased_always_on   # OTel-spec sampler kinds
      # sampler_arg: 0.1               # only used by *_traceidratio
      traces:
        batch:
          max_queue_size: 2048
          flush_ms: 5000
      metrics:
        enabled: true
        export_interval_ms: 60000
        api_key_id_cap: 1024  # only applies to *metric* dimensions; spans always carry the raw value
        user_id_cap: 256
```

### Environment Variables

Standard OpenTelemetry environment variables are supported and take precedence:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_HEADERS="x-api-key=secret"
export OTEL_SERVICE_NAME=bitrouter
export OTEL_RESOURCE_ATTRIBUTES="deployment.environment=prod,team=platform"
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

## Tenant Attribution

Every trace span and metric automatically includes:

- `bitrouter.api_key_id`: The API key used for authentication
- `bitrouter.user_id`: The user who owns the API key
- `bitrouter.account_label`: Which provider account served the request (for multi-account providers)

This enables:
- Per-tenant request rates, latency, and error tracking
- Usage-based billing and cost attribution
- Tenant-specific debugging and tracing

## Cardinality Management

High-cardinality dimensions are automatically capped to prevent metric explosion:

- API key IDs over the limit (default 1024) are bucketed as "other"
- User IDs over the limit (default 256) are bucketed as "other"
- This protects your observability backend from cardinality explosion

## Migration from Prometheus

### Option 1: Prometheus 3.x Native OTLP

```yaml
# prometheus.yml
scrape_configs:
  - job_name: bitrouter
    otlp:
      endpoint: http://bitrouter:4318
```

### Option 2: OpenTelemetry Collector

```yaml
# otel-collector.yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  prometheus:
    endpoint: 0.0.0.0:8889

service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [prometheus]
```

### Option 3: Use OTLP-Native Backends

Most modern observability platforms support OTLP directly:
- Honeycomb
- Datadog
- New Relic
- Grafana Cloud
- AWS X-Ray
- Google Cloud Trace

## Testing

### Local Testing with Jaeger

```bash
# Start Jaeger with OTLP support
docker run -d --name jaeger \
  -e COLLECTOR_OTLP_ENABLED=true \
  -p 16686:16686 \
  -p 4318:4318 \
  jaegertracing/all-in-one:latest

# Configure bitrouter
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318

# View traces at http://localhost:16686
```

### Performance Benchmarks

```bash
cd plugins/bitrouter-observe
cargo bench --features otel
```

Benchmarks measure:
- Baseline (no observability) vs with observability
- Cardinality capping performance
- Concurrent request handling overhead

### Test Script

Use the included test script for comprehensive testing:

```bash
./scripts/test_observability.sh
```

This will:
1. Start a local Jaeger instance
2. Configure bitrouter with test settings
3. Generate multi-tenant test traffic
4. Run performance benchmarks
5. Display results in Jaeger UI

## Span Structure

Two spans per request. Span names follow the GenAI semconv:
`{gen_ai.operation.name} {gen_ai.request.model}`.

```
chat <model>                           (root, SpanKind=SERVER)
├── bitrouter.request_id
├── bitrouter.api_key_id               (raw value — capping is metrics-only)
├── bitrouter.user_id
├── bitrouter.provider_id
├── bitrouter.account_label            (multi-account providers)
├── bitrouter.latency_ms
├── bitrouter.generation_time_ms
├── bitrouter.outcome                  (completed | failed | disconnected)
├── gen_ai.operation.name              ("chat")
├── gen_ai.system                      (provider_id)
├── gen_ai.request.model
├── gen_ai.response.model
├── gen_ai.response.finish_reasons     (array)
├── gen_ai.usage.input_tokens
├── gen_ai.usage.output_tokens
├── gen_ai.usage.reasoning_tokens      (when > 0)
├── error.type                         (on failures)
└── error.message                      (on failures)
    │
    └── bitrouter.execution            (child, SpanKind=CLIENT)
        ├── bitrouter.route_chain_length
        ├── bitrouter.target_provider
        └── bitrouter.target_model
```

Inbound W3C `traceparent` is parsed and used as the parent context.

## Metrics

Standard GenAI-semconv instruments:

- `gen_ai.client.operation.duration` — histogram, seconds
- `gen_ai.client.token.usage` — counter, split by `gen_ai.token.type` (`input` / `output`)

Plus a small set of router-specific instruments:

- `bitrouter.requests` — counter (with `outcome`)
- `bitrouter.errors` — counter
- `bitrouter.stream_parts` — counter (with `part_type`)

Each metric carries the tenant attribute set:
- `api_key_id` (cardinality-capped)
- `user_id` (cardinality-capped)
- `gen_ai.system` (provider id)
- `gen_ai.response.model`
- `outcome` (completed / failed / disconnected)
- `bitrouter.account_label` (multi-account providers)

## Not yet implemented

- `bitrouter observe status` CLI + `DaemonCommand::ObserveStatus`
- `AppReloader::reload` rebuilding the observe hook stack
- OTLP/gRPC and OTLP/HTTP+JSON protocol selection (only HTTP+protobuf today)
- Log correlation via trace IDs (waiting on `opentelemetry-appender-tracing`)
- Per-tenant OTLP fan-out (lives in `bitrouter-cloud`, not here)

## License

Part of the BitRouter project. See repository root for license information.