# BitRouter - Open Intelligence Router for LLM Agents

> The zero-ops LLM gateway built for modern agent runtime. Single binary. Zero infrastructure dependencies. Agent-native control.

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

# Start the default interactive experience (TUI + API server)
bitrouter
```

If you only want the foreground API server, run:

```bash
bitrouter serve
```

If you want to use the foreground TUI app, run:

```bash
bitrouter
```

If you want to start the local proxy service at background, run:

```bash
bitrouter start
```

## CLI Overview

`bitrouter` has two ways to run:

- `bitrouter` starts the default interactive runtime. With the default `tui` feature enabled, this launches the TUI and API server together.
- `bitrouter [COMMAND]` runs an explicit operational command.

### Subcommands

| Command   | What it does                                                                  |
| --------- | ----------------------------------------------------------------------------- |
| `serve`   | Start the API server in the foreground                                        |
| `start`   | Start BitRouter as a background daemon                                        |
| `stop`    | Stop the running daemon                                                       |
| `status`  | Print resolved paths, listen address, configured providers, and daemon status |
| `restart` | Restart the background daemon                                                 |

### Global path options

These flags are available on the top-level command and on each subcommand:

- `--home-dir <PATH>` — override BitRouter home directory resolution
- `--config-file <PATH>` — override `<home>/bitrouter.yaml`
- `--env-file <PATH>` — override `<home>/.env`
- `--run-dir <PATH>` — override `<home>/run`
- `--logs-dir <PATH>` — override `<home>/logs`

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
- [ ] MCP & A2A protocol support
- [ ] TUI observability dashboard
- [ ] Telemetry and usage analytics
- [ ] Provider & model routing policy customization

## License

Licensed under the [Apache License 2.0](LICENSE).
