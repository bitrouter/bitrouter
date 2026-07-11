---
title: Codex
description: Route OpenAI's Codex CLI through BitRouter by registering it as a custom model provider.
sourceHash: c6042bc984b397d5a4214b910fabd2a09a71e8f1afb02672a747d1db8939a399
---

Codex CLI can run through BitRouter without taking over your Codex config. The quickest path is `bitrouter spawn --agent codex`, which injects one-shot Codex config overrides for that child process only; if you prefer a permanent setup, register BitRouter as a custom provider in `~/.codex/config.toml`.

## Prerequisites

- BitRouter running — local proxy at `http://127.0.0.1:4356`, or [BitRouter Cloud](/docs/get-started/configuration) at `https://api.bitrouter.ai`.
- Codex installed:

  ```bash
  curl -fsSL https://chatgpt.com/codex/install.sh | sh
  ```

## Launch with spawn

```bash
bitrouter spawn --agent codex
```

Everything after `--` is forwarded to Codex:

```bash
bitrouter spawn --agent codex -- --model openai/gpt-5-codex
```

Before launching a long run, ask BitRouter to check the route it will use:

```bash
bitrouter spawn --agent codex --check -- --model openai/gpt-5-codex
```

The check verifies that `codex` is installed, the BitRouter base URL is reachable, and the forwarded model has at least one Responses-compatible endpoint. It does **not** require the provider to be `openai-codex`; any model source that routes over the Responses protocol can be used by the Codex harness.

`spawn` does not edit `~/.codex/config.toml`. It starts from your existing Codex model selection, then points Codex at a transient `bitrouter` provider with `base_url = "<BitRouter>/v1"` and `wire_api = "responses"`. If `BITROUTER_API_KEY` is exported, Codex uses it through `env_key`; otherwise BitRouter injects a local placeholder that works with the `skip_auth: true` default written by `bitrouter init`.

<Callout type="warn">
Avoid forwarding Codex `-c` / `--config` flags through `bitrouter spawn --agent codex`. Current Codex releases can let those forwarded config flags override the transient provider injection, which means the run may silently stop using BitRouter. `spawn` rejects that shape and asks you to move the option into Codex config or inspect the route with `--check`.
</Callout>

## Permanent config

Add a provider block to `~/.codex/config.toml`. The `base_url` includes `/v1` — Codex appends the route (`/responses`) itself.

```toml
model_provider = "bitrouter"

[model_providers.bitrouter]
name = "BitRouter"
base_url = "http://127.0.0.1:4356/v1"
wire_api = "responses"
# env_key = "BITROUTER_API_KEY"  # Cloud or authenticated local daemon
```

```bash
codex
```

<Callout type="info">
**No key for the local proxy** — omit `env_key` and Codex sends no auth, which the loopback proxy accepts under `skip_auth: true`. For **Cloud**, set `base_url = "https://api.bitrouter.ai/v1"`, add `env_key = "BITROUTER_API_KEY"` to the block, and export that variable with your BitRouter key.
</Callout>

<Callout type="warn">
`model_provider` / `model_providers` only take effect in the **user-level** `~/.codex/config.toml`, not a project-local `.codex/config.toml`.
</Callout>

## Pick a model

The Codex `model` setting, or a per-run `codex --model <id>`, can use any registry id in `provider/model` form — `openai/gpt-5-codex`, `anthropic/claude-sonnet-4-6`, `google/gemini-2.5-pro` — optionally with a `:cost` or `:latency` variant. See [Models](/docs/concepts/models).

## Verify

Run `codex` and issue a prompt; check BitRouter's `request finished` log line (`~/.bitrouter/bitrouter.log` for a local install) to see which `provider` and `model` answered.

## Learn more

- [Codex — configuration reference](https://developers.openai.com/codex/config-reference)
- [Model fallback](/docs/features/model-fallback) · [Provider selection](/docs/features/provider-selection)
