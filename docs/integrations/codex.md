---
title: Codex
description: Route OpenAI's Codex CLI through BitRouter by registering it as a custom model provider.
sourceHash: c6042bc984b397d5a4214b910fabd2a09a71e8f1afb02672a747d1db8939a399
---

Codex CLI lets you register a custom OpenAI-compatible provider in `~/.codex/config.toml`. Add BitRouter as that provider and Codex routes through the whole [registry](/docs/concepts/models) instead of only OpenAI.

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- Codex installed:

  ```bash
  npm install -g @openai/codex
  ```

## Point Codex at BitRouter

Add a provider block to `~/.codex/config.toml`. The `base_url` includes `/v1` — Codex appends the route (`/chat/completions`) itself.

```toml
model = "openai/gpt-4o"
model_provider = "bitrouter"

[model_providers.bitrouter]
name = "BitRouter"
base_url = "http://127.0.0.1:4356/v1"
wire_api = "chat"          # Chat Completions; use "responses" to target /v1/responses
```

```bash
codex
```

<Callout type="info">
**No key for the local proxy** — omit `env_key` and Codex sends no auth, which the loopback proxy accepts. For **Cloud**, set `base_url = "https://api.bitrouter.ai/v1"`, add `env_key = "BITROUTER_API_KEY"` to the block, and export that variable with your BitRouter key.
</Callout>

<Callout type="warn">
`model_provider` / `model_providers` only take effect in the **user-level** `~/.codex/config.toml`, not a project-local `.codex/config.toml`.
</Callout>

## Pick a model

The `model` field takes any registry id in `provider/model` form — `openai/gpt-4o`, `anthropic/claude-sonnet-4-6`, `google/gemini-2.5-pro` — optionally with a `:cost` or `:latency` variant. Override per run with `codex --model <id>`. See [Models](/docs/concepts/models).

## Verify

Run `codex` and issue a prompt; check the `bitrouter-served-by` response header to see which provider answered.

## Learn more

- [Codex — configuration reference](https://developers.openai.com/codex/config-reference)
- [Model fallback](/docs/features/model-fallback) · [Provider selection](/docs/features/provider-selection)
