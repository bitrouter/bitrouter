# Migrate from LiteLLM

LiteLLM's `model_list` maps cleanly to BitRouter's `providers:` (credentials + base URLs) plus `models:` (your aliases). Your client code mostly only needs the base URL flipped.

> **Cloud alternative:** if the user is open to managed routing instead of self-hosting, the equivalent of "stop running LiteLLM" is "point your SDK at `https://api.bitrouter.ai/v1` with a `brk_*` key" — no daemon, no YAML, no provider keys. See `references/cloud-setup.md`. The rest of this file covers the self-hosted (local daemon) migration.

## The shape of the move

| LiteLLM | BitRouter |
|---|---|
| `model_list[].model_name` | `models.<alias>` |
| `model_list[].litellm_params.model` (e.g. `openai/gpt-4o`) | `models.<alias>.upstream_id` (same `provider/model` form) |
| `model_list[].litellm_params.api_key` | `providers.<id>.api_key` (or omit — built-ins auto-resolve from env) |
| `model_list[].litellm_params.api_base` | `providers.<id>.api_base` (only needed for custom endpoints) |
| Default port `:8000` | `:4356` |

## Example translation

**Old `litellm_config.yaml`**

```yaml
model_list:
  - model_name: gpt-4
    litellm_params:
      model: openai/gpt-4o
      api_key: ${OPENAI_API_KEY}
  - model_name: claude
    litellm_params:
      model: anthropic/claude-sonnet-4-5
      api_key: ${ANTHROPIC_API_KEY}
  - model_name: fast
    litellm_params:
      model: anthropic/claude-haiku-4-5
```

**New `bitrouter.yaml`**

```yaml
server:
  listen: "127.0.0.1:4356"
  skip_auth: true

providers:
  openai: {}         # uses OPENAI_API_KEY
  anthropic: {}      # uses ANTHROPIC_API_KEY

models:
  gpt-4:
    upstream_id: "openai/gpt-4o"
  claude:
    upstream_id: "anthropic/claude-sonnet-4-5"
  fast:
    upstream_id: "anthropic/claude-haiku-4-5"

inherit_defaults: true
```

## Client switch

```python
# old
from litellm import completion
response = completion(model="gpt-4", messages=[...])

# new — same alias, OpenAI SDK against bitrouter
from openai import OpenAI
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
response = client.chat.completions.create(model="gpt-4", messages=[...])
```

## Cutover

```bash
# 1. start bitrouter alongside the running litellm
bitrouter start

# 2. verify aliases resolve
bitrouter route gpt-4
bitrouter route claude

# 3. smoke test
curl http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4","messages":[{"role":"user","content":"ping"}],"max_tokens":5}'

# 4. flip your app's base URL from :8000 to :4356
# 5. stop litellm
pkill -f 'litellm' || true
```

## Mapping LiteLLM features

| LiteLLM | BitRouter equivalent |
|---|---|
| `master_key` | `bitrouter key sign --user <id>` (mint `brvk_…` virtual keys); set `server.skip_auth: false` |
| `router_settings.fallbacks` | Provider `accounts` with `account_strategy: failover`, or per-pattern `derives` |
| `router_settings.routing_strategy: simple-shuffle` | `account_strategy: balance` on a multi-account provider |
| `litellm_params.rpm` / `tpm` | `providers.<id>.rate_limits` (glob-prefix pattern map) |
| Callbacks / logging | `bitrouter observe status`; OTel exporter via env (`OTEL_EXPORTER_OTLP_ENDPOINT`) |

For richer routing (`derives`, multi-account, per-pattern `api_protocol`), see `providers.md`.

## Docker Compose

```yaml
# old
services:
  litellm:
    image: ghcr.io/berriai/litellm:latest
    ports: ["8000:8000"]

# new — pin a tag in production
services:
  bitrouter:
    image: ghcr.io/bitrouter/bitrouter:latest    # TODO: confirm image path
    ports: ["4356:4356"]
    volumes:
      - ~/.bitrouter:/root/.bitrouter
    environment:
      - OPENAI_API_KEY
      - ANTHROPIC_API_KEY
      - GEMINI_API_KEY
      - OPENROUTER_API_KEY
```

If anything fails verification, see `diagnose.md`.
