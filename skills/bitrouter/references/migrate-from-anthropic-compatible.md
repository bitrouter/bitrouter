# Migrate from an Anthropic-compatible source

Use this when your current setup speaks **Anthropic Messages** on the wire — directly against `api.anthropic.com`, against Amazon Bedrock's Anthropic surface, against Google Vertex's Anthropic surface, or against any third-party that exposes `POST /v1/messages`.

Generic move: point the client at `http://localhost:4356` (no `/v1`) instead of the upstream, let BitRouter hold the credential, and the SDK keeps working unchanged.

> **Cloud alternative:** same move, different host — point the client at `https://api.bitrouter.ai` (no `/v1`) with a `brk_*` key. No daemon, no Anthropic key to manage. See `references/cloud-setup.md`. The rest of this file is the self-hosted path.

## A) Direct Anthropic (raw API key, no proxy)

If `ANTHROPIC_API_KEY` is in your environment, no config file needed.

```bash
export ANTHROPIC_API_KEY=sk-ant-...
bitrouter start
```

```python
# old
from anthropic import Anthropic
client = Anthropic()

# new — same SDK, different base
client = Anthropic(base_url="http://localhost:4356", api_key="unused")
client.messages.create(
    model="anthropic/claude-sonnet-4-5",
    max_tokens=256,
    messages=[{"role": "user", "content": "hi"}],
)
```

The registry-backed `anthropic` provider auto-enabled when the env var was set.

## B) Amazon Bedrock

`aws-bedrock` is a registry-backed provider. It reaches Bedrock's OpenAI-compatible
`bedrock-mantle` endpoints (not the SigV4 Converse API), so all it needs is a
Bedrock API key:

```bash
export AWS_BEARER_TOKEN_BEDROCK=...    # generate a Bedrock API key in the AWS console
export AWS_REGION=us-west-2            # optional; defaults to us-east-1
bitrouter reload                       # or restart
bitrouter providers list               # aws-bedrock → ACTIVE
```

Claude models on Bedrock route as e.g. `anthropic/claude-opus-4.8`
(`bitrouter models --provider aws-bedrock` is authoritative). Native-Converse-only
features (cross-region inference profiles, Bedrock Guardrails, structured outputs)
are not served through this surface.

## C) Google Vertex AI

**Claude-on-Vertex is not covered.** The registry-backed `vertex` provider runs in
Vertex AI **Express Mode** (`VERTEX_EXPRESS_API_KEY`), which serves **Gemini
models only** — it does not serve Anthropic Claude (or Llama/Mistral) on Vertex.
Those partner models are present but **commented out** in the `vertex` registry
entry, pending service-account OAuth support. Those partner models live on Vertex's regional
endpoints and require a short-lived Google OAuth access token (minted per hour
from a service-account key), which needs provider-specific code BitRouter does
not ship today.

So for this Anthropic-shaped migration: if your Claude traffic runs through
Vertex, there's no drop-in registry provider yet — use Anthropic's own API (`anthropic`)
or Bedrock (`aws-bedrock`) instead, or run Claude-on-Vertex behind your own
proxy and add it as a custom `anthropic`-protocol provider (see section D).
Full native Vertex support (partner models + service-account auth) is a possible
future addition.

## D) Any other Anthropic-Messages-shaped endpoint

Some self-hosted gateways and aggregators (LiteLLM in `/messages` mode, Helicone in pass-through, a custom proxy your team runs) speak Anthropic Messages. Add them as a custom provider with `api_protocol` set to `anthropic`:

```yaml
providers:
  internal-claude:
    api_base: "https://claude.internal.example.com"
    api_key: "${INTERNAL_CLAUDE_KEY}"
    api_protocol: { "*": anthropic }
    models:
      - { id: "claude-sonnet-4-5" }
      - { id: "claude-haiku-4-5" }
```

## Client switch (universal)

```python
# old
from anthropic import Anthropic
client = Anthropic(api_key=os.environ["ANTHROPIC_API_KEY"])

# new
client = Anthropic(base_url="http://localhost:4356", api_key="unused")
```

> **Note the missing `/v1`.** The Anthropic SDK appends `/v1/messages` itself. The OpenAI SDK, by contrast, expects the base to end in `/v1` — that's why the two SDKs use different BitRouter base URLs.

## Cross-protocol bonus

Once you're proxying through BitRouter, the same daemon can serve the **OpenAI** SDK against the **Anthropic** provider (and vice versa) — BitRouter cross-protocol-routes. So you can keep the Anthropic SDK in code that already uses it, and use the OpenAI SDK against `model="anthropic/claude-sonnet-4-5"` in code that wants one client.

## Verify

```bash
bitrouter start
bitrouter providers list                  # anthropic active: yes
bitrouter route anthropic/claude-sonnet-4-5

curl http://localhost:4356/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model":"anthropic/claude-sonnet-4-5",
    "max_tokens":5,
    "messages":[{"role":"user","content":"ping"}]
  }'
```

If anything fails verification, see `diagnose.md`.
