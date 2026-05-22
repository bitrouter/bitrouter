# BitRouter CLI Reference

`bitrouter <subcommand> [flags]`

## Config resolution

All subcommands accept an optional `-c / --config <path>` flag. When omitted the binary walks this order:

1. `./bitrouter.yaml` in the current directory
2. `$BITROUTER_HOME/bitrouter.yaml` — if the env var is set, the file must exist
3. `~/.bitrouter/bitrouter.yaml` — used if present
4. **Zero-config** — in-memory defaults; auto-enables any provider whose API key is set in the environment

Daemon-control subcommands (`stop`, `reload`, `status`) also accept `--socket <path>` to override the control socket path derived from the config.

---

## Daemon lifecycle

### `bitrouter serve`

Run the HTTP server and control socket **in the foreground**.

```
bitrouter serve [-c <path>]
```

Starts the proxy on the configured listen address (default `127.0.0.1:8787`) and opens a Unix domain control socket. Logs to stdout.

### `bitrouter start`

Spawn `serve` as a **detached background daemon**.

```
bitrouter start [-c <path>] [--log <path>]
```

Logs default to `~/.bitrouter/bitrouter.log`. Refuses to start if a daemon is already running.

### `bitrouter stop`

```
bitrouter stop [-c <path>] [--socket <path>]
```

### `bitrouter restart`

```
bitrouter restart [-c <path>] [--socket <path>] [--log <path>]
```

Stops the running daemon (waiting up to 30s for in-flight requests to drain), then starts a fresh one.

### `bitrouter reload`

```
bitrouter reload [-c <path>] [--socket <path>]
```

Hot-reloads the running daemon's config and routing table without dropping connections. Also triggered by `SIGHUP`.

Any provider API keys present in the current environment are forwarded to the daemon so `export OPENAI_API_KEY=…; bitrouter reload` takes effect immediately.

### `bitrouter status`

```
bitrouter status [-c <path>] [--socket <path>]
```

Prints pid, listen address, number of routable models, and control socket path. Exits cleanly with "stopped" when no daemon is reachable.

---

## Config

### `bitrouter init`

```
bitrouter init [-c <path>]           # default: ./bitrouter.yaml
```

Writes a commented starter `bitrouter.yaml` with `skip_auth: true`. Edit it to configure providers, routing, guardrails, MCP servers, and agents.

---

## Routing / introspection

### `bitrouter route <model>`

```
bitrouter route gpt-4o [-c <path>] [--socket <path>]
```

Resolves a model name through the routing table and prints the full fallback chain (provider → upstream service id → protocol). Queries the running daemon if reachable; falls back to a local config parse.

### `bitrouter models`

```
bitrouter models [-c <path>] [-p <provider-id>]
```

Lists all routable models. Filter by provider with `--provider`.

### `bitrouter providers list`

```
bitrouter providers list [-c <path>]
```

Prints each configured provider's id, model count, active state, and API base URL.

---

## MCP tool introspection

### `bitrouter tools list`

```
bitrouter tools list [-c <path>]
```

Connects to every `mcp_servers` entry in the config and lists advertised tools with descriptions.

### `bitrouter tools status`

```
bitrouter tools status [-c <path>]
```

Health-checks each configured MCP server with a `tools/list` round-trip. Prints status, latency, and transport.

### `bitrouter tools discover <server>`

```
bitrouter tools discover my-server [-c <path>]
```

Connects to one MCP server and prints a YAML stub suitable for pasting into the `mcp_servers:` block of `bitrouter.yaml`.

---

## ACP agent management

### `bitrouter agents list`

```
bitrouter agents list [-c <path>]
```

Shows the built-in agent catalog alongside which agents are configured in the loaded config.

### `bitrouter agents check`

```
bitrouter agents check [-c <path>]
```

Spawns each configured agent and verifies it responds to `initialize`. Prints latency or error per agent.

### `bitrouter agents install <id>`

```
bitrouter agents install claude-code
```

Prints a YAML stub for the named catalog agent. Paste the output under `agents:` in `bitrouter.yaml`.

### `bitrouter agent-proxy <id>`

```
bitrouter agent-proxy claude-code [-c <path>]
```

Stdio bridge between an ACP-aware editor and a configured upstream agent. The editor spawns this as a child process; `agent-proxy` routes JSON-RPC over the `acp` pipeline and relays notifications back.

---

## Auth and keys

### `bitrouter key sign`

```
bitrouter key sign --user <id> [--db <url>] [--policy <policy-id>]
```

Mints a scoped `brvk_` virtual key for a user. The plaintext secret is printed once — only its SHA-256 hash is stored.

| Flag | Default | Description |
| --- | --- | --- |
| `--user` | *(required)* | Owning user id |
| `--db` | `sqlite://./bitrouter.db` | Database URL — `sqlite://`, `postgres://`, or `mysql://` |
| `--policy` | *(none)* | Policy id to bind to the key |

### `bitrouter login <provider>`

```
bitrouter login github-copilot
```

Runs the provider's OAuth Device Authorization Grant and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`.

### `bitrouter logout <provider>`

```
bitrouter logout github-copilot
```

Removes the stored OAuth token for the provider.

---

## Policy

### `bitrouter policy create <id>`

```
bitrouter policy create strict [--dir ./policies]
```

Writes a starter policy file to the policy directory. Bind it to a key with:

```
bitrouter key sign --user <id> --policy strict
```
