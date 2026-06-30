---
title: Self-host BitRouter
description: A production walkthrough for running self-hosted BitRouter — config file, keys, daemon lifecycle, metrics, and hardening.
sourceHash: 8c7accdceb945d6dfd258bdc9d2e9117263e3bc41481462feb3e65831502810b
---

# Self-host BitRouter

This is the **production** path for running BitRouter on your own infrastructure:
a committed config file, real provider keys, the router running as a managed
daemon, metrics export, and basic hardening. If you just want it running in 60
seconds, start with [Installation](/docs/get-started/installation) — this guide
picks up where that leaves off. Deciding between self-host and the hosted product?
See [Self-host vs Cloud](/docs/get-started/self-hosted-vs-cloud).

The router listens on `127.0.0.1:4356` by default — loopback only, until you
explicitly choose otherwise.

## 1. Generate and structure `bitrouter.yaml`

Scaffold a commented starter file:

```bash
bitrouter init                 # writes ./bitrouter.yaml
bitrouter init -c /etc/bitrouter/bitrouter.yaml
```

`bitrouter init` writes a starter config with `skip_auth: true`. Edit it to
configure providers, routing, and the rest. Treat `bitrouter.yaml` as
infrastructure-as-code: commit it, review changes, and keep secrets out of it
(use `${VAR}` references, resolved from the environment at load time).

The config is keyed into top-level sections — the ones you'll touch first are
`server`, `providers`, and `models`:

```yaml
# yaml-language-server: $schema=https://bitrouter.dev/schema/v<VERSION>/config.schema.json

server:
  # Loopback by default; set 0.0.0.0 only when you intend to expose the router
  # on all interfaces.
  listen: 127.0.0.1:4356
  log_level: info

providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: ${OPENAI_API_KEY}
    models:
      - id: gpt-4o

  anthropic:
    api_base: https://api.anthropic.com
    api_key: ${ANTHROPIC_API_KEY}
    # `api_protocol` is a glob-prefix pattern list: the head of each set is the
    # preferred outbound protocol.
    api_protocol:
      - "*": messages
    models:
      - id: claude-sonnet-4-6

# A virtual model that fails over from one provider to another in declared
# order (the default `priority` strategy).
models:
  smart:
    strategy: priority
    endpoints:
      - provider: anthropic
        service_id: claude-sonnet-4-6
      - provider: openai
        service_id: gpt-4o
```

**Structure that matters:**

- `providers` is a **map keyed by provider id** (`openai`, `anthropic`, …). Each
  entry takes `api_base` (upstream base URL), `api_key` (usually a `${VAR}`
  reference), an optional `api_protocol` pattern list, and a `models` list whose
  entries each require an `id`.
- `api_protocol` selects the outbound wire protocol per provider — e.g.
  `messages` for Anthropic. Known values include `chat_completions`, `messages`,
  `generate_content`, and `responses`.
- `models` declares **virtual models**: named aliases with a `strategy` (default
  `priority`) and an ordered list of `endpoints`, each pointing at a
  `provider` + `service_id`. This is how you get failover.
- `server` carries `listen`, `log_level`, an optional `skip_auth`, and an optional
  `control_socket` path.

<Callout type="info">
The full set of top-level sections is `server`, `providers`, `models`,
`presets`, `variants`, `mcp`, `mcp_servers`, `agents`, `server_tools`,
`database`, `plugins`, and `inherit_defaults`. The authoritative schema lives at
`schemas/bitrouter.config.schema.json` in the core repo, and the
`# yaml-language-server` header gives editors autocomplete and inline validation.
</Callout>

Validate before you ship:

```bash
bitrouter config validate -c bitrouter.yaml
```

## 2. Environment and keys

Secrets stay in the environment, never in the committed file. The `${VAR}`
placeholders in `bitrouter.yaml` are resolved at load time:

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
```

In production, deliver these through your process manager's environment (a systemd
`EnvironmentFile`, a secrets mount, etc.) rather than a shell profile. When you
rotate a key, you don't need to restart — `bitrouter reload` forwards any provider
API keys present in the current environment to the running daemon, so
`export OPENAI_API_KEY=…; bitrouter reload` takes effect immediately.

<Callout type="warn">
`bitrouter init` writes `skip_auth: true`, which admits credential-less requests.
That is fine for local development behind loopback, but for a production
deployment that is reachable by anything other than localhost you must put
authentication in front of the router (a reverse proxy / gateway, or a
deployment that implements its own auth hook). Do not expose a `skip_auth: true`
router on `0.0.0.0` without an auth layer.
</Callout>

## 3. Run as a daemon

For production you want the router running detached and supervised. The daemon
lifecycle commands (verified against the [CLI reference](/docs/concepts/cli)):

```bash
bitrouter start        # spawn `serve` as a detached background daemon
bitrouter status       # pid, listen address, routable model count, socket path
bitrouter reload       # hot-reload config + routing table (also via SIGHUP)
bitrouter restart      # drain in-flight requests (up to 30s), then start fresh
bitrouter stop         # stop the daemon
```

- `bitrouter serve` runs the server in the **foreground** (logs to stdout) — this
  is what you point a systemd unit or container entrypoint at.
- `bitrouter start` spawns a **detached** daemon and refuses to start if one is
  already running. Logs default to `bitrouter.log` next to the config file.
- `bitrouter reload` hot-reloads config and the routing table **without dropping
  connections**.

All commands accept `-c / --config <path>`; the daemon-control commands
(`stop`, `reload`, `status`) also accept `--socket <path>` to override the Unix
control socket. Under a process supervisor, prefer `bitrouter serve` as the
foreground entrypoint and let the supervisor handle restarts:

```ini
# /etc/systemd/system/bitrouter.service
[Service]
ExecStart=/usr/local/bin/bitrouter serve -c /etc/bitrouter/bitrouter.yaml
EnvironmentFile=/etc/bitrouter/bitrouter.env
Restart=on-failure
```

## 4. Export telemetry

BitRouter is OpenTelemetry-native: the `bitrouter-observe` plugin pushes traces
and metrics over **OTLP** (HTTP or gRPC) to any OpenTelemetry backend. Point it
at your collector with an `otel` block (or the matching env vars):

```yaml
plugins:
  bitrouter-observe:
    otel:
      endpoint: "http://otel-collector:4318"
      service_name: "bitrouter"
```

There is no Prometheus scrape endpoint — metrics are pushed via OTLP only. For a
Prometheus-based stack, ingest through an OpenTelemetry Collector. Confirm the
exporter is live with `bitrouter observe status`. You can also trace how a model
name resolves before it ever hits an upstream:

```bash
bitrouter route gpt-4o     # print the full fallback chain for a model
```

See [OpenTelemetry](/docs/features/opentelemetry) for the span model,
per-request attribution, and per-backend export configs.

## 5. Basic hardening

- **Bind deliberately.** Keep `listen: 127.0.0.1:4356` unless you have a reason to
  expose it. If you set `0.0.0.0`, put a reverse proxy with TLS and auth in front.
- **Don't ship `skip_auth: true`** on any non-loopback deployment (see §2).
- **Keep secrets in the environment.** Only `${VAR}` references belong in the
  committed `bitrouter.yaml`; the config redacts `api_key` in debug output.
- **Validate in CI.** Run `bitrouter config validate -c bitrouter.yaml` on every
  change, and pin the `# yaml-language-server` schema URL to your version.
- **Add a content firewall** with [Guardrails](/docs/features/guardrails) to
  block or redact request/response content.
- **Reload, don't restart, for config changes** so in-flight requests aren't
  dropped.

## Next steps

<Cards>
  <Card title="Installation" href="/docs/get-started/installation" description="The 60-second local install, if you skipped it." />
  <Card title="Self-host vs Cloud" href="/docs/get-started/self-hosted-vs-cloud" description="Pick the deployment model that fits." />
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="OTLP trace + metric export and per-request attribution." />
  <Card title="Guardrails" href="/docs/features/guardrails" description="Content firewall for requests and responses." />
</Cards>
