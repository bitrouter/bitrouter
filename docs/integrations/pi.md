---
title: Pi
description: Route Pi, the minimal terminal coding harness, through BitRouter as an OpenAI-compatible provider.
sourceHash: a5270f935b7221c1a9ea6a9b6d2ebf4686ac0901c38a7afb85f27ae62fe9463b
---

Pi is a minimal terminal coding harness that defines model providers in `~/.pi/agent/models.json`. Add BitRouter as an OpenAI-compatible provider and Pi routes across the whole [registry](/docs/concepts/models).

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/quickstart) at `https://api.bitrouter.ai`.
- Pi installed:

  ```bash
  npm install -g --ignore-scripts @earendil-works/pi-coding-agent
  ```

## Point Pi at BitRouter

Add a provider to `~/.pi/agent/models.json`. Use `api: "openai-completions"` and point `baseUrl` at the `/v1` root.

```json
{
  "providers": {
    "bitrouter": {
      "baseUrl": "http://127.0.0.1:4356/v1",
      "api": "openai-completions",
      "apiKey": "local-placeholder",
      "models": [
        { "id": "openai/gpt-4o", "name": "GPT-4o via BitRouter" },
        { "id": "anthropic/claude-sonnet-4-6", "name": "Claude Sonnet 4.6 via BitRouter" }
      ]
    }
  }
}
```

Then pick the model with `/model` in the TUI.

<Callout type="info">
**No key for the local proxy** — `apiKey` can be any placeholder. For **Cloud**, set `baseUrl` to `https://api.bitrouter.ai/v1` and use your BitRouter key (the field also accepts `$ENV_VAR` references).
</Callout>

## Pick a model

Each `models[].id` is a registry id in `provider/model` form, optionally with a `:cost` / `:latency` variant. See [Models](/docs/concepts/models).

## Learn more

- [Pi — custom providers](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md)
- [Model fallback](/docs/features/model-fallback)
