# Migrate from OpenRouter

OpenRouter is a cloud aggregator; BitRouter is a local proxy. You don't have to choose — BitRouter has a built-in `openrouter` provider, so the cleanest migration runs both side-by-side and lets you decide later whether to keep OpenRouter for the long tail or rip it out.

> **Cloud-for-cloud alternative:** if the user wants to swap one managed service for another (skip local hosting entirely), point them at `https://api.bitrouter.ai/v1` with a `brk_*` key — see `references/cloud-setup.md`. The rest of this file covers self-hosted (local daemon) migration.

## Path A: keep OpenRouter as a fallback (recommended)

Direct providers in front (faster, cheaper, no markup); OpenRouter behind for models you don't have direct access to.

```yaml
server:
  listen: "127.0.0.1:4356"
  skip_auth: true

providers:
  openai: {}         # uses OPENAI_API_KEY
  anthropic: {}      # uses ANTHROPIC_API_KEY
  google: {}         # uses GEMINI_API_KEY
  openrouter: {}     # uses OPENROUTER_API_KEY — built-in

models:
  # alias OpenRouter-only models so client code stays clean
  llama-70b:
    upstream_id: "openrouter/meta-llama/llama-3.1-70b-instruct"
  qwen-72b:
    upstream_id: "openrouter/qwen/qwen-2.5-72b-instruct"

inherit_defaults: true
```

Once direct providers are credentialed, point your code at the BitRouter alias (`gpt-4o`, `claude-sonnet-4-5`, etc.) and OpenRouter only gets hit when you ask for an OpenRouter-specific model.

## Path B: replace OpenRouter entirely

If you only used OpenRouter for first-party models (OpenAI, Anthropic, Google), drop `openrouter` from `providers:` and credential the direct providers instead. Code-side, swap the model identifiers:

```python
# old: OpenRouter prefixes everything with the upstream
model="openai/gpt-4o"
model="anthropic/claude-3.5-sonnet"

# new: same form, but routed locally and only billed once
model="openai/gpt-4o"
model="anthropic/claude-sonnet-4-5"
```

## Client switch

```python
# old
client = OpenAI(
    base_url="https://openrouter.ai/api/v1",
    api_key=os.environ["OPENROUTER_API_KEY"],
)

# new — credential lives in the daemon, not the client
client = OpenAI(
    base_url="http://localhost:4356/v1",
    api_key="unused",
)
```

If you used OpenRouter's stat-tracking headers (`HTTP-Referer`, `X-Title`), they pass through unchanged when you keep the `openrouter` provider in BitRouter — the daemon forwards request headers untouched.

## Verify

```bash
bitrouter start
bitrouter providers list                  # openrouter should show active: yes
bitrouter route openrouter/meta-llama/llama-3.1-70b-instruct
bitrouter route openai/gpt-4o             # direct, not via openrouter

curl http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"llama-70b","messages":[{"role":"user","content":"ping"}],"max_tokens":5}'
```

## Cost-aware aliasing

A common reason to migrate is to stop paying OpenRouter's markup on models you can hit directly. The pattern:

```yaml
models:
  # client asks for "gpt-4o" — bitrouter hits openai directly,
  # not openrouter/openai/gpt-4o
  gpt-4o:
    upstream_id: "openai/gpt-4o"
  claude-sonnet:
    upstream_id: "anthropic/claude-sonnet-4-5"
```

Then your application code keeps requesting `gpt-4o` / `claude-sonnet`, and you've stripped the cloud aggregator off the path without touching the client.

If anything fails verification, see `diagnose.md`.
