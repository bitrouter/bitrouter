# Harness: Codex CLI

Wire OpenAI's Codex CLI to route its model calls through BitRouter.

> **Cloud users:** swap `http://localhost:4356/v1` -> `https://api.bitrouter.ai/v1` and export `BITROUTER_API_KEY=brk_*`. No daemon to install. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed and running (`bitrouter status` shows green), unless using Cloud.
- Codex CLI installed (`curl -fsSL https://chatgpt.com/codex/install.sh | sh`).
- A BitRouter model id to use, such as `openai/gpt-5-codex`, `openai/gpt-5.1`, or any configured alias.

## Preferred launch path

Use `bitrouter spawn` when you want a reversible, per-process setup:

```bash
bitrouter spawn --agent codex
bitrouter spawn --agent codex -- --model openai/gpt-5-codex
```

The wrapper does not edit `~/.codex/config.toml`. It injects one-shot Codex `-c` overrides:

```text
model_provider="bitrouter"
model_providers.bitrouter.name="BitRouter"
model_providers.bitrouter.base_url="http://localhost:4356/v1"
model_providers.bitrouter.wire_api="responses"
```

If `BITROUTER_API_KEY` is set, `spawn` forwards it with `env_key="BITROUTER_API_KEY"`. Otherwise it injects a local placeholder bearer token, which works with the `skip_auth: true` default from `bitrouter init`.

## Permanent Codex config

For a durable setup, add a user-level provider to `~/.codex/config.toml`:

```toml
model_provider = "bitrouter"

[model_providers.bitrouter]
name = "BitRouter"
base_url = "http://localhost:4356/v1"
wire_api = "responses"
# env_key = "BITROUTER_API_KEY"  # Cloud or authenticated local daemon
```

Codex appends `/responses` to the provider base URL. Do not use `wire_api = "chat"` with current Codex builds.

## Model selection

Codex's `model` setting or `codex --model <id>` can be any BitRouter registry id. `bitrouter spawn --agent codex` deliberately does not force a model; it only changes the provider so the configured or forwarded model routes through BitRouter.

```bash
codex --model openai/gpt-5-codex
bitrouter spawn --agent codex -- --model anthropic/claude-sonnet-4-6
```

## Verify

```bash
codex --version
bitrouter spawn --agent codex -- --version
tail -n 20 ~/.bitrouter/bitrouter.log
```

For live requests, check the BitRouter request logs (`~/.bitrouter/bitrouter.log`) to confirm which upstream provider answered.

## Agent plugin

The BitRouter agent plugin (repo root `.codex-plugin/`) layers onto this wiring for Codex users: a `SessionStart` hook that reports routing status + a spend recap, the origin MCP server for in-session model arbitrage (bundled MCP servers must be enabled manually on Codex after install), and a session spend summary printed by `bitrouter spawn` on exit. See `references/agent-plugin.md`.
