---
title: Claude Code
description: Route Anthropic's Claude Code through BitRouter — every model in the registry, behind the Anthropic Messages API it already speaks.
sourceHash: ba5716929b662eb242b1293fadd37f20d416a2af82f6926e0c1d4c57d5ebd953
---

Claude Code talks to an Anthropic Messages endpoint. Point it at BitRouter instead of `api.anthropic.com` and every request flows through the [registry](/docs/concepts/models) — so the same agent can run on Anthropic, OpenAI, Google, or an open model, with provider selection and fallback underneath.

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- Claude Code installed:

  ```bash
  npm install -g @anthropic-ai/claude-code
  # or the native installer:
  curl -fsSL https://claude.ai/install.sh | bash
  ```

## Point Claude Code at BitRouter

Claude Code reads three environment variables. Set the base URL to BitRouter's **host root** — it appends `/v1/messages` itself.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:4356
export ANTHROPIC_AUTH_TOKEN=local-placeholder        # sent as a bearer token
export ANTHROPIC_MODEL=anthropic/claude-sonnet-4-6
claude
```

<Callout type="info">
**No key for the local proxy.** The local proxy accepts loopback requests without auth, so `ANTHROPIC_AUTH_TOKEN` can be any placeholder. For **Cloud**, set `ANTHROPIC_BASE_URL=https://api.bitrouter.ai` and use your BitRouter key (from `bitrouter auth login` or the dashboard) as the token.
</Callout>

Prefer a file? Put the same values in `.claude/settings.json` so they apply per-project without exporting:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4356",
    "ANTHROPIC_AUTH_TOKEN": "local-placeholder",
    "ANTHROPIC_MODEL": "anthropic/claude-sonnet-4-6"
  }
}
```

## Pick a model

`ANTHROPIC_MODEL` takes any registry id in `provider/model` form — `anthropic/claude-sonnet-4-6`, `openai/gpt-4o`, `google/gemini-2.5-pro`. Add a `:cost` or `:latency` suffix to bias provider selection for the session. Switch mid-session with `/model <id>`, or at launch with `claude --model <id>`. See [Models](/docs/concepts/models) for the full id scheme.

## Verify

Launch `claude`, ask it anything, and confirm the response. BitRouter's `bitrouter-served-by` response header tells you which provider actually answered.

## Learn more

- [Claude Code — LLM gateway config](https://code.claude.com/docs/en/llm-gateway)
- [Model fallback](/docs/features/model-fallback) — pass an ordered model list and walk it on failure.
- [OpenTelemetry](/docs/features/opentelemetry) — trace every Claude Code request.
