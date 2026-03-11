# BitRouter - Open Intelligence Router for LLM Agents

> The zero-ops LLM gateway built for modern agent runtime. Single binary. Zero infrastructure dependencies. Agent-native control.

[![Build status](https://github.com/bitrouter/bitrouter/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/bitrouter/bitrouter/actions)
[![Crates.io](https://img.shields.io/crates/v/bitrouter)](https://crates.io/crates/bitrouter)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Twitter](https://img.shields.io/badge/Twitter-black?logo=x&logoColor=white)](https://x.com/BitRouterAI)
[![Discord](https://img.shields.io/badge/Discord-5865F2?logo=discord&logoColor=white)](https://discord.gg/G3zVrZDa5C)
[![Docs](https://img.shields.io/badge/Docs-bitrouter.ai-green)](https://bitrouter.ai)

## Overview

As LLM agents grow more autonomous, humans can no longer hand-pick the best model, tool, or sub-agent for every runtime decision. BitRouter is a proxy layer purpose-built for LLM agents (OpenClaw, OpenCode, etc.) to discover and route to LLMs, tools, and other agents autonomously — with agent-native control and observability via CLI + TUI, backed by a high-performance Rust proxy that optimizes for both performance and cost during runtime.

## Features

- **Agent-native routing** — agents discover and select LLMs, tools, and sub-agents at runtime
- **Multi-provider gateway** — unified access to OpenAI, Anthropic, Google, and more
- **Streaming & non-streaming** — first-class support for both modes
- **CLI + TUI observability** — monitor and control agent sessions in real time
- **Smart routing** — cost and performance optimization via configurable routing tables
- **High-performance proxy** — single Rust binary, async-first, minimal overhead
- **Tool calling** — unified tool use across providers

## Documentation

- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and server composition details
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution workflow, issue reporting, and provider updates
- [`CLAUDE.md`](CLAUDE.md) — guidance for AI coding agents working in this repository

## Quick Start

```bash
# Install
cargo install bitrouter

# Launch — runs the setup wizard on first run, then starts the TUI + API server
bitrouter
```

On first launch, if no providers are configured, BitRouter automatically runs an interactive setup wizard that walks you through provider selection, API key entry, and configuration. After setup completes, the TUI and API server start with your new configuration.

You can also run the setup wizard explicitly at any time:

```bash
bitrouter init
```

For a headless API server (no TUI):

```bash
bitrouter --headless
```

To run a single foreground server command explicitly:

```bash
bitrouter serve
```

To run as a background daemon:

```bash
bitrouter start
```

### Zero-config mode

If you have provider API keys in your environment (e.g. `OPENAI_API_KEY`), BitRouter auto-detects them and enables direct routing without any configuration file:

```bash
export OPENAI_API_KEY=sk-...
bitrouter serve
# Use "openai:gpt-4o" as the model name
```

## CLI Overview

`bitrouter` has two ways to run:

- `bitrouter` starts the default interactive runtime. On first run with no providers configured, the setup wizard runs automatically. With the default `tui` feature enabled, this then launches the TUI and API server together.
- `bitrouter --headless` starts the default runtime without the TUI.
- `bitrouter [COMMAND]` runs an explicit operational command.

### Subcommands

| Command   | What it does                                                                  |
| --------- | ----------------------------------------------------------------------------- |
| `init`    | Interactive setup wizard for provider configuration                           |
| `serve`   | Start the API server in the foreground                                        |
| `start`   | Start BitRouter as a background daemon                                        |
| `stop`    | Stop the running daemon                                                       |
| `status`  | Print resolved paths, listen address, configured providers, and daemon status |
| `restart` | Restart the background daemon                                                 |
| `account` | Manage local Ed25519 account keypairs used to sign BitRouter JWTs             |
| `keygen`  | Sign a JWT with the active account key                                        |
| `keys`    | List, inspect, and remove locally stored JWTs                                 |

### Global options

These flags are available on the top-level command and on each subcommand:

- `--home-dir <PATH>` — override BitRouter home directory resolution
- `--config-file <PATH>` — override `<home>/bitrouter.yaml`
- `--env-file <PATH>` — override `<home>/.env`
- `--run-dir <PATH>` — override `<home>/run`
- `--logs-dir <PATH>` — override `<home>/logs`
- `--db <DATABASE_URL>` — override the database URL from environment variables and config

Top-level runtime flags:

- `--headless` — run the default runtime without the TUI

### Local account and JWT helpers

BitRouter can generate and manage local Ed25519 account keys under `<home>/.keys`, then use the active account to mint JWTs for API access:

```bash
# Generate a local account keypair and set it active
bitrouter account --generate-key

# Create an API token for that account and save it locally
bitrouter keygen --exp 30d --models openai:gpt-4o --name default

# Inspect or remove saved tokens
bitrouter keys --list
bitrouter keys --show default
bitrouter keys --rm default
```

## Configuration and `BITROUTER_HOME`

BitRouter resolves its working directory in this order:

1. `--home-dir <PATH>` if provided
2. The current working directory, if `./bitrouter.yaml` exists
3. `BITROUTER_HOME`, if it points to an existing directory
4. `~/.bitrouter`

When BitRouter falls back to `~/.bitrouter`, it scaffolds the directory if needed.

### Default home layout

```text
<home>/
├── bitrouter.yaml
├── .env
├── .gitignore
├── logs/
└── run/
```

The scaffolded `.gitignore` ignores `logs/`, `run/`, and `.env`. The runtime automatically loads `<home>/.env` when it exists, then reads `<home>/bitrouter.yaml`.

### Minimal configuration

The easiest way to create a configuration is to run `bitrouter init`, which generates `bitrouter.yaml` and `.env` interactively. You can also write the config manually:

```yaml
server:
  listen: 127.0.0.1:8787

providers:
  openai:
    api_key: ${OPENAI_API_KEY}

models:
  default:
    strategy: priority
    endpoints:
      - provider: openai
        model_id: gpt-4o
```

Provider definitions are merged on top of BitRouter's built-in provider registry, so you can start by overriding only the fields you need. Environment-variable references like `${OPENAI_API_KEY}` are expanded during config loading.

### Custom providers

`bitrouter init` supports adding custom OpenAI-compatible or Anthropic-compatible providers. You can also define them manually in `bitrouter.yaml`:

```yaml
providers:
  openrouter:
    derives: openai
    api_base: "https://openrouter.ai/api/v1"
    api_key: "${OPENROUTER_API_KEY}"
  moonshot-anthropic:
    derives: anthropic
    api_base: "https://api.moonshot.ai/anthropic"
    api_key: "${MOONSHOT_API_KEY}"
```

The `derives` field inherits protocol handling from the named built-in provider, so any service with an OpenAI-compatible or Anthropic-compatible API works out of the box.

## Supported Providers

| Provider  | Status | Notes                            |
| --------- | ------ | -------------------------------- |
| OpenAI    | ✅     | Chat Completions + Responses API |
| Anthropic | ✅     | Messages API                     |
| Google    | ✅     | Generative AI API                |

Want to see another provider supported? [Open an issue](https://github.com/AIMOverse/bitrouter/issues) or submit a PR — contributions are welcome. If you're a provider interested in first-party integration, reach out on [Discord](https://discord.gg/G3zVrZDa5C).

## Roadmap

- [x] Core routing engine and provider abstractions
- [x] OpenAI, Anthropic, and Google adapters
- [x] Interactive setup wizard (`bitrouter init`) with auto-detection
- [x] Custom provider support (OpenAI-compatible / Anthropic-compatible)
- [x] Cross-protocol routing (e.g. OpenAI format → Anthropic provider)
- [ ] MCP & A2A protocol support
- [ ] TUI observability dashboard
- [ ] Telemetry and usage analytics
- [ ] Provider & model routing policy customization

## License

Licensed under the [Apache License 2.0](LICENSE).
