---
title: OpenCode
description: Route OpenCode through BitRouter by adding it as an OpenAI-compatible provider in opencode.json.
sourceHash: 94d4da78f9eb3926cce3a90d1b8f45caafb742f1b47a7715fec61618f3391bd2
---

OpenCode registers providers through `opencode.json` using the AI SDK. Add BitRouter as an OpenAI-compatible provider and the terminal agent routes across the whole [registry](/docs/concepts/models).

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- OpenCode installed:

  ```bash
  curl -fsSL https://opencode.ai/install | bash
  # or: npm install -g opencode-ai
  ```

## Point OpenCode at BitRouter

Add a provider block to `opencode.json` (project root or `~/.config/opencode/opencode.json`). `baseURL` points at the `/v1` root; the model keys are the registry ids you want to expose.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "bitrouter": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "BitRouter",
      "options": {
        "baseURL": "http://127.0.0.1:4356/v1",
        "apiKey": "{env:BITROUTER_API_KEY}"
      },
      "models": {
        "openai/gpt-4o": { "name": "GPT-4o via BitRouter" },
        "anthropic/claude-sonnet-4-6": { "name": "Claude Sonnet 4.6 via BitRouter" }
      }
    }
  }
}
```

```bash
opencode
```

<Callout type="info">
**No key for the local proxy** — `apiKey` can be any placeholder. For **Cloud**, set `baseURL` to `https://api.bitrouter.ai/v1` and supply your BitRouter key (via the `{env:...}` reference above, or `opencode auth login`).
</Callout>

## Pick a model

Each key under `models` is a registry id in `provider/model` form, optionally with a `:cost` / `:latency` variant. Select one in the TUI, or run `opencode run --model bitrouter/openai/gpt-4o`. See [Models](/docs/concepts/models).

## Learn more

- [OpenCode — providers](https://opencode.ai/docs/providers/) · [config](https://opencode.ai/docs/config/)
- [Model fallback](/docs/features/model-fallback)
