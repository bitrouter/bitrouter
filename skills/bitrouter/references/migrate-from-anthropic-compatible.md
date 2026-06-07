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

The built-in `anthropic` provider auto-enabled when the env var was set.

## B) Amazon Bedrock (Anthropic surface)

Bedrock exposes Anthropic models behind AWS SigV4 auth, not bearer tokens — different enough that the v1 BitRouter has a dedicated `bitrouter-bedrock` crate. To check what the v1 surface accepts in your build:

```bash
bitrouter providers list
# look for `bedrock` or similar
```

> **TODO** — confirm the exact v1 provider id and config shape for Bedrock. Check `crates/bitrouter-bedrock` in the bitrouter repo, or <https://bitrouter.ai>. The general pattern is:

```yaml
providers:
  bedrock:
    # placeholder — verify in v1 docs
    api_base: "https://bedrock-runtime.us-east-1.amazonaws.com"
    api_protocol: { "*": anthropic }
    # AWS credentials come from the standard provider chain
    # (env vars, ~/.aws/credentials, IAM role)
```

## C) Google Vertex AI (Anthropic surface)

Vertex serves Claude models with Google IAM auth. Same caveat as Bedrock — confirm the v1 provider id:

```yaml
providers:
  vertex:
    # placeholder — verify in v1 docs
    api_base: "https://us-central1-aiplatform.googleapis.com/v1/projects/PROJECT/locations/us-central1/publishers/anthropic/models"
    api_protocol: { "*": anthropic }
```

> **TODO** — fill in once verified against the codebase.

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
