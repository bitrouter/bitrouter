# bitrouter

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Command-line entry point for BitRouter.

This crate builds the `bitrouter` binary and exposes the top-level operational
commands used to run or control the service. It wires CLI parsing to the
runtime crate and keeps the executable layer intentionally thin.

## First-Run Behavior

When `bitrouter` is launched with no subcommand and no providers are configured,
the setup wizard runs automatically before starting the TUI. This guides new
users through provider selection, API key entry, and configuration file
generation. After setup, the runtime reloads and the TUI launches with the
new configuration.

If the user cancels the wizard, the TUI launches in its empty state.

The TUI integrates with the [Agent Client Protocol (ACP)](https://agentclientprotocol.com/)
to manage coding agent sessions. It auto-discovers ACP-compatible agents on
PATH (e.g. `claude-agent-acp`, `openclaw`) and connects via JSON-RPC over stdio.
See [`bitrouter-tui`](../bitrouter-tui/) for details on supported agents and
adapter installation.

## CLI Overview

`bitrouter` has two ways to run:

- `bitrouter` starts the default interactive runtime. On first run with no providers configured, the setup wizard runs automatically. With the default `tui` feature enabled, this then launches the TUI and API server together.
- `bitrouter [COMMAND]` runs an explicit operational command such as `serve` for a foreground server or `start` for a background daemon.

### Subcommands

| Command        | What it does                                                                  |
| -------------- | ----------------------------------------------------------------------------- |
| `serve`        | Start the API server in the foreground                                        |
| `start`        | Start BitRouter as a background daemon                                        |
| `stop`         | Stop the daemon                                                               |
| `status`       | Print resolved paths, listen address, configured providers, and daemon status |
| `restart`      | Restart the background daemon                                                 |
| `reload`       | Hot-reload the configuration file without restarting                          |
| `wallet`       | Manage OWS wallets (create, import, list, info, export, delete, rename)       |
| `key`          | Manage OWS API keys for agent access (create, list, revoke, sign)             |
| `policy`       | Manage spend-limit policies for OWS wallet signing                            |
| `auth`         | Manage provider authentication (login, refresh, status)                       |
| `route`        | Manage runtime routes on a running daemon (list, add, rm)                     |
| `tools`        | Inspect MCP tools on a running daemon (list, status, discover)                |
| `models`       | List routable models                                                          |
| `agents`       | List available ACP agents                                                     |
| `agent-proxy`  | Run as ACP stdio proxy for a configured agent                                 |
| `reset`        | Wipe configuration and re-run the setup wizard                                |

### Global options

These flags are available on the top-level command and on each subcommand:

- `--home-dir <PATH>` — override BitRouter home directory resolution
- `--config-file <PATH>` — override `<home>/bitrouter.yaml`
- `--env-file <PATH>` — override `<home>/.env`
- `--run-dir <PATH>` — override `<home>/run`
- `--logs-dir <PATH>` — override `<home>/logs`
- `--db <DATABASE_URL>` — override the database URL from environment variables and config

### Wallet and key management

BitRouter manages OWS wallets and API keys for agent access under `<home>`:

```bash
# Create a wallet
bitrouter wallet create --name default

# Create an API key bound to a wallet
bitrouter key create --name claude-agent --wallet default

# Sign a JWT for agent access
bitrouter key sign --wallet default --exp 30d --models openai:gpt-4o

# Store the JWT on the running server and print a short virtual key instead
bitrouter key sign --wallet default --exp 30d --models openai:gpt-4o --virtual-key

# List and revoke keys
bitrouter key list
bitrouter key revoke --id <key-id>
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
