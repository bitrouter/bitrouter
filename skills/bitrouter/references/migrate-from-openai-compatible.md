# Migrate from an OpenAI-compatible source

Use this when your current setup speaks **OpenAI Chat Completions** (or Responses) on the wire — directly against `api.openai.com`, against Azure OpenAI, or against any third-party that exposes that surface (Together, Groq, Perplexity, Fireworks, DeepInfra, vLLM, Ollama, LM Studio, …).

Generic move: point the client at `http://localhost:4356/v1` instead of the upstream, let BitRouter hold the credential, and the rest of the SDK keeps working.

> **Cloud alternative:** the same generic move works for BitRouter Cloud — point the client at `https://api.bitrouter.ai/v1` with a `brk_*` key. No daemon, no provider keys to manage. See `references/cloud-setup.md`. The rest of this file is the self-hosted path.

## A) Direct OpenAI (raw API key, no proxy)

The simplest case. If `OPENAI_API_KEY` is in your environment, no config file is needed.

```bash
export OPENAI_API_KEY=sk-...
bitrouter start
```

```python
# old
client = OpenAI(api_key=os.environ["OPENAI_API_KEY"])

# new
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
client.chat.completions.create(model="openai/gpt-4o", messages=[...])
```

That's it — the built-in `openai` provider auto-enabled when the env var was set.

## B) Azure OpenAI

Azure's per-deployment naming maps onto BitRouter's `models[]` list. Credentials and base URL go on a custom provider:

```yaml
providers:
  azure:
    api_base: "https://YOUR_RESOURCE.openai.azure.com"
    api_key: "${AZURE_OPENAI_KEY}"
    models:
      - { id: "gpt-4o",        upstream_id: "gpt4-deployment" }
      - { id: "gpt-4o-mini",   upstream_id: "gpt4-mini-deployment" }
      - { id: "text-embedding-3-large", upstream_id: "embedding-deployment" }
```

Then:

```python
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
client.chat.completions.create(model="azure/gpt-4o", messages=[...])
```

> Azure's `api-version` query parameter and per-region routing aren't covered by the BitRouter v1 schema in detail — confirm against the live docs at <https://bitrouter.ai> if your setup needs them.

## C) Together / Groq / Fireworks / Perplexity / DeepInfra

All of these are OpenAI-shaped behind their own host. Pattern:

```yaml
providers:
  together:
    api_base: "https://api.together.xyz/v1"
    api_key: "${TOGETHER_API_KEY}"
    auto_discover: true            # pull /v1/models at startup + reload

  groq:
    api_base: "https://api.groq.com/openai/v1"
    api_key: "${GROQ_API_KEY}"
    auto_discover: true

  perplexity:
    api_base: "https://api.perplexity.ai"
    api_key: "${PERPLEXITY_API_KEY}"
    models:
      - { id: "llama-3.1-sonar-large-128k-online" }
```

`auto_discover: true` lets BitRouter learn the model catalog from `/v1/models` at startup and on reload — useful for providers that ship new models frequently.

Client side, nothing changes vs. raw OpenAI — only the model identifier changes:

```python
client.chat.completions.create(model="groq/llama-3.1-70b-versatile", messages=[...])
```

## D) Local: Ollama / LM Studio / vLLM

No credential needed (these are usually unauthenticated on localhost). Add as a custom provider and either list models explicitly or let auto-discovery fill them in.

```yaml
providers:
  ollama:
    api_base: "http://localhost:11434/v1"
    models:
      - { id: "llama3.1:70b" }
      - { id: "codellama:34b" }
      - { id: "qwen2.5:32b" }
    tags: [local, free]

  lmstudio:
    api_base: "http://localhost:1234/v1"
    auto_discover: true
    tags: [local, free]
```

Tags help with use-case routing — `RoutingPrefs.require_tags: [local]` keeps requests off the cloud.

## E) Anything else OpenAI-shaped

Same recipe: drop in a custom provider with `api_base` + `api_key` (or omit for unauthenticated local endpoints), list models or set `auto_discover: true`. If the endpoint uses a non-standard request format, it's not actually OpenAI-compatible — open an issue at <https://github.com/bitrouter/bitrouter/issues> for adapter support, or write a `provider/model`-specific `api_protocol` mapping in your config.

## Client switch (universal)

```python
# whatever you were using before, the new line is:
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
```

The Anthropic SDK works too if you happen to call Claude models — point it at `http://localhost:4356` (no `/v1`) and BitRouter cross-protocol-routes for you.

## Verify

```bash
bitrouter start
bitrouter providers list                  # all the providers you added should be active
bitrouter models                          # routable surface
bitrouter route azure/gpt-4o              # whatever id you expect

curl http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"azure/gpt-4o","messages":[{"role":"user","content":"ping"}],"max_tokens":5}'
```

If anything fails verification, see `diagnose.md`.
