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
      traces:
        include_bodies: false
        batch:
          max_spans: 512
          flush_ms: 5000
      metrics:
        enabled: true
        export_interval_ms: 60000
        api_key_id_cap: 1024  # Limit unique API keys
        user_id_cap: 256      # Limit unique user IDs
```

### Environment Variables

Standard OpenTelemetry environment variables are supported and take precedence:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_HEADERS="x-api-key=secret"
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_SERVICE_NAME=bitrouter
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

MVP design with two spans per request:

```
bitrouter.request (root span)
├── Attributes:
│   ├── bitrouter.request_id
│   ├── bitrouter.model
│   ├── bitrouter.api_key_id (capped)
│   ├── bitrouter.user_id (capped)
│   ├── bitrouter.provider_id
│   ├── bitrouter.account_label (if multi-account)
│   ├── bitrouter.latency_ms
│   ├── gen_ai.usage.input_tokens
│   └── gen_ai.usage.output_tokens
│
└── bitrouter.execution (child span)
    └── Attributes:
        ├── bitrouter.route_chain_length
        ├── bitrouter.target_provider
        └── bitrouter.target_model
```

## Metrics

The following metrics are exported with tenant dimensions:

- `bitrouter.requests`: Request counter with outcome
- `bitrouter.request.latency`: Latency histogram in milliseconds
- `bitrouter.tokens`: Token usage counter
- `bitrouter.errors`: Error counter
- `bitrouter.stream_parts`: Stream parts processed
- `bitrouter.otel.spans_dropped`: Observability health metric
- `bitrouter.otel.metrics_dropped`: Observability health metric

Each metric includes dimensions:
- `api_key_id` (cardinality capped)
- `user_id` (cardinality capped)  
- `provider_id`
- `model`
- `outcome` (completed/failed/disconnected)
- `account_label` (for multi-account providers)

## Future Enhancements (Not in MVP)

- [ ] CLI integration (`bitrouter observe status`)
- [ ] Configurable sampling ratios
- [ ] OTLP/gRPC protocol support
- [ ] OTLP/HTTP+JSON protocol support
- [ ] Log correlation via trace IDs
- [ ] Custom span attributes via config
- [ ] Per-tenant OTLP routing (stays in bitrouter-cloud)

## License

Part of the BitRouter project. See repository root for license information.