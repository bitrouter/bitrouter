---
title: OpenTelemetry
description: BitRouter is OpenTelemetry-native — traces and metrics for every request, exported over OTLP to any backend you run. Everything here is open-source and runs on your own infrastructure.
sourceHash: bdb0ade9182ea76246cb218b8b1d126497ab54b4ab6cd22ed66cdb606e6f8d1a
---

BitRouter is **OpenTelemetry-native**. Every request you send through the router becomes a **trace** — the full lifecycle from ingress through routing, each upstream attempt (including failovers), and settlement — plus a set of **metrics**, all following the [OpenTelemetry GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) and pushed over OTLP to any backend you already run.

Everything on this page is **open-source** and runs entirely on your own infrastructure — there's no BitRouter telemetry endpoint in the middle. It's **off until you point it somewhere**, and it excludes message content by default. If you'd rather not run a collector, [BitRouter Cloud](/docs/features/opentelemetry#cloud-activity-hosted) gives you a hosted request view with nothing to operate.

## How a trace looks

Each request produces a span tree:

```
HTTP SERVER  POST /v1/chat/completions        (ingress)
└─ chat      (INTERNAL, inbound — whole request lifetime)
   ├─ route  (INTERNAL — routing decision)
   ├─ chat   (CLIENT — upstream attempt #1, gen_ai.* attributes)
   ├─ chat   (CLIENT — failover attempt #2)
   └─ settle (INTERNAL — settlement summary)
```

There is **one GenAI generation per request** — the inbound `chat` span. Each
upstream attempt is a separate `CLIENT` span, so a failover chain shows every
provider it tried, in order, with the latency and outcome of each hop. BitRouter
extracts inbound W3C trace context and injects an outbound `traceparent`, so
router spans stitch into the parent trace from your agent or gateway. Because
each attempt is its own span, traces are where failover and routing behavior
become legible: which provider was tried first, why it fell through, and where
latency went across the chain.

### Span attributes

| Attribute | Description |
| --- | --- |
| `gen_ai.provider.name` | Upstream provider for the hop (e.g. `openai`, `anthropic`) |
| `gen_ai.response.model` | Model that actually served the response |
| `gen_ai.token.type` | `input` / `output`, on token measurements |
| `outcome` | Final disposition of the request |
| `api_key_id`, `user_id` | Caller attribution (cardinality-capped) |
| `account_label` | Logical account/tenant label |

## Enable export

Add an `otel` block under the `bitrouter-observe` plugin and give it an endpoint.
That alone turns export on:

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "http://localhost:4318"   # your OTLP endpoint
      service_name: "bitrouter"
```

Keep secrets out of the committed file — use `${VAR}` references for any auth
headers, resolved from the environment at load time:

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "https://api.honeycomb.io"
      headers:
        x-honeycomb-team: "${HONEYCOMB_API_KEY}"
```

Every field has an environment-variable override, so you can configure export
without touching the file — useful in containers, where you can run with no
`otel` block at all:

| Env var | Sets |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP endpoint URL |
| `OTEL_EXPORTER_OTLP_HEADERS` | Auth headers (comma-separated `k=v`) |
| `OTEL_SERVICE_NAME` | Resource service name |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes (comma-separated `k=v`) |
| `OTEL_TRACES_SAMPLER` | Sampler kind |
| `OTEL_TRACES_SAMPLER_ARG` | Sampler argument (e.g. ratio) |
| `BITROUTER_OBSERVE_CONTENT_CAPTURE` | Content capture mode |

<Callout type="info">
The OTLP **transport** is selected when the binary is built: `otel-http`
(OTLP/HTTP + protobuf, the default) or `otel-grpc` (OTLP/gRPC). The configuration
on this page is identical for both — you only care about the transport if your
backend speaks one and not the other.
</Callout>

## Metrics

Alongside traces, metrics are exported over OTLP on an interval (default 60s):

| Metric | Type | Measures |
| --- | --- | --- |
| `bitrouter.requests` | Counter | Requests processed |
| `gen_ai.client.operation.duration` | Histogram | Request latency |
| `gen_ai.client.token.usage` | Histogram | Token counts (by `gen_ai.token.type`) |
| `bitrouter.errors` | Counter | Errors |
| `bitrouter.stream_parts` | Counter | Streaming parts emitted |

Dimensions include `gen_ai.provider.name`, `gen_ai.response.model`, `outcome`,
`account_label`, and the caller identifiers. To keep metric cardinality bounded
on shared deployments, `api_key_id` and `user_id` are capped (defaults: 1024 and
256 distinct values); beyond the cap, values collapse to an overflow bucket.

<Callout type="warn">
There is **no Prometheus scrape endpoint**. `GET /metrics` is retired — metrics
are pushed via OTLP only. If your stack is Prometheus-based, ingest through an
OpenTelemetry Collector with the Prometheus exporter.
</Callout>

## Backend recipes

Each block is the `plugins.bitrouter-observe.otel` config for a common backend.

### OpenTelemetry Collector

Send everything to a local or in-cluster Collector and let it fan out to your
real backends (this is also the path to a Prometheus-based stack — the Collector's
Prometheus exporter bridges the gap, since BitRouter has no scrape endpoint):

```yaml
otel:
  endpoint: "http://otel-collector:4318"
  service_name: "bitrouter"
  resource_attributes:
    deployment.environment: "prod"
```

### Honeycomb

```yaml
otel:
  endpoint: "https://api.honeycomb.io"
  service_name: "bitrouter"
  headers:
    x-honeycomb-team: "${HONEYCOMB_API_KEY}"
```

### Grafana Cloud / Tempo

Grafana Cloud's OTLP gateway uses basic auth (instance ID + API token, base64
encoded). For self-hosted Tempo, point at its OTLP port and drop the header.

```yaml
otel:
  endpoint: "https://otlp-gateway-<region>.grafana.net/otlp"
  service_name: "bitrouter"
  headers:
    Authorization: "Basic ${GRAFANA_OTLP_TOKEN}"
```

### Datadog

Datadog ingests OTLP through the Datadog Agent rather than a public OTLP URL —
run the Agent with OTLP receiving enabled and point BitRouter at it:

```yaml
otel:
  endpoint: "http://datadog-agent:4318"
  service_name: "bitrouter"
  resource_attributes:
    deployment.environment: "prod"
```

## Tune sampling

By default BitRouter respects the inbound trace decision and otherwise samples
everything (`parentbased_always_on`). On high throughput, sample a fraction
instead:

```yaml
otel:
  endpoint: "http://otel-collector:4318"
  sampler: "parentbased_traceidratio"
  sampler_arg: 0.1                       # keep 10% of root traces
```

| `sampler` | Behavior |
| --- | --- |
| `always_on` | Sample every trace |
| `always_off` | Sample nothing |
| `traceidratio` | Sample a fraction (`sampler_arg`), ignoring parent |
| `parentbased_always_on` | Follow parent; sample if no parent *(default)* |
| `parentbased_always_off` | Follow parent; drop if no parent |
| `parentbased_traceidratio` | Follow parent; otherwise sample `sampler_arg` |

`parentbased_*` variants honor the upstream decision, so a trace your agent
started won't be half-sampled at the router. The metrics export interval and the
trace batch queue are tunable separately under `metrics` and `traces.batch` if
you need to trade freshness for overhead.

## Content capture

Prompt and response **content is excluded by default** (`content_capture: off`).
Turn it on only when you need prompt and response bodies on the spans for
debugging:

```yaml
otel:
  content_capture: "full"   # off (default) | full
```

<Callout type="warn">
`full` writes user prompts and model responses into your telemetry backend.
That content then inherits the backend's access controls and retention. For
shared or regulated environments, leave it `off` and capture content only in a
scoped, short-lived debugging session.
</Callout>

## Verify

Reload (or restart) the router, then ask the running daemon what it's doing:

```bash
bitrouter reload                  # pick up config changes without dropping connections
bitrouter observe status          # endpoint, sampler, cardinality, in-flight spans
bitrouter observe status --json
```

If it reports `stopped`, the exporter isn't wired — check that the `otel` block
has an `endpoint` (or that `OTEL_EXPORTER_OTLP_ENDPOINT` is set) and that the
binary was built with an OTLP transport feature. Then send a request through the
router and confirm the trace lands in your backend; you should see one inbound
`chat` span per request with a `CLIENT` child for each upstream attempt.

## Next steps

<Cards>
  <Card title="Cloud Tracing" href="/docs/features/opentelemetry#cloud-activity-hosted" description="Hosted request view — spend, tokens, and a per-request log, nothing to operate." />
  <Card title="Self-host BitRouter" href="/docs/guides/self-host" description="Run the router in production, with telemetry wired in." />
  <Card title="Model fallback" href="/docs/features/model-fallback" description="The failover chains you'll see in every trace." />
  <Card title="Guardrails" href="/docs/features/guardrails" description="Content firewall for requests and responses." />
</Cards>

## Cloud Activity (hosted)

The open-source [OpenTelemetry](/docs/features/opentelemetry) export runs on your own backend. **BitRouter Cloud** gives you the hosted alternative: every `/v1` request is traced into an **Activity** view server-side — no collector, no warehouse, nothing to run. Content (prompts and responses) is never stored.

### The Activity dashboard

Sign in to [cloud.bitrouter.ai](https://cloud.bitrouter.ai) and open **Activity**. It opens on three KPI cards over a window you pick — **1 day**, **1 week**, **1 month**, or **all time**:

| KPI | What it measures |
| --- | --- |
| **Spend** | Total USD charged over the window |
| **Requests** | Number of requests over the window |
| **Tokens** | Prompt + completion tokens over the window |

Every figure is scoped to the **active workspace** ([namespace](/docs/features/namespaces)), so a dashboard always reflects the workspace you're signed into.

### The request log

Below the KPIs, the request log lists every `/v1` request, newest first. Each row is a per-request trace record:

| Column | Detail |
| --- | --- |
| **Time** | When the request landed |
| **Model** | The model id served, with a `stream` marker for streamed calls |
| **Provider** | The upstream provider that served it |
| **Tokens** | Prompt + completion total |
| **Cost** | Final charge in USD |
| **Latency** | End-to-end latency |
| **Source** | Funding source (credit balance, BYOK, MPP session) |
| **Status** | Succeeded, error, denied, cancelled |

Each record also carries the **routing profile** used (`balanced`, `cost`, `latency`, `throughput`) and the gated **capabilities** exercised (e.g. `structured_outputs`) — so a request that failed over or hit a budget is legible without leaving the dashboard.

<Callout type="info">
**Receipts, not bodies.** Cloud stores the request *record* — model, provider, tokens, cost, latency, status, routing profile — never the prompt or response content.
</Callout>

### Usage attribution & the API

Everything in the dashboard is also available over the management API, scoped per workspace and gated by the `usage:read` scope:

- **Aggregate usage** — spend, token counts, request count, and a per-capability breakdown over a `[from, to)` window.
- **Request history** — the paginated request log, including routing profile and capabilities used.

These are the same `bitrouter cloud usage` and `bitrouter cloud requests` commands you run from the [CLI](/docs/concepts/cli). See the [API Reference](/docs/reference) for the `usage` and `requests` endpoints and their fields.

### Deep traces

Cloud stores per-request **receipts**, not OpenTelemetry span waterfalls. When you need the full span tree — the ingress span, the routing decision, and a `CLIENT` span per upstream attempt — that lives in **your own OTLP collector**. Wire it up once with the open-source [OpenTelemetry](/docs/features/opentelemetry) export and the Activity view links out to it.

### Next steps

<Cards>
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="Self-run OTLP export — the span model, metrics, and backend recipes." />
  <Card title="Workspaces" href="/docs/features/namespaces" description="Per-workspace scoping for keys, usage, and policy." />
  <Card title="CLI" href="/docs/concepts/cli" description="bitrouter cloud usage / requests from the terminal." />
  <Card title="API Reference" href="/docs/reference" description="The usage and requests management endpoints." />
</Cards>
