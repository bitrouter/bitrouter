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

Starts the proxy on the configured listen address (default `127.0.0.1:4356`) and opens a Unix domain control socket. Logs to stdout.

### `bitrouter start`

Spawn `serve` as a **detached background daemon**.

```
bitrouter start [-c <path>] [--log <path>]
```

Logs default to `bitrouter.log` next to the config file (e.g. `~/.bitrouter/bitrouter.log` when the config resolved to `~/.bitrouter/bitrouter.yaml`). Refuses to start if a daemon is already running.

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
bitrouter login anthropic            # Claude Pro/Max subscription PKCE flow
bitrouter login openai-codex         # ChatGPT subscription PKCE flow
bitrouter login github-copilot       # GitHub device-code flow
```

Runs the provider's OAuth flow (PKCE in a browser or device-code, depending on provider) and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The slot is keyed by `(provider_id, label)` — pass `--label <name>` (defaults to `default`) to keep multiple accounts of the same provider side by side. Other providers fall back to a pasted API key.

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bitrouter auth login`](#bitrouter-auth-login--logout--whoami) below.

### `bitrouter logout <provider>`

```
bitrouter logout github-copilot
```

Removes every stored credential for the provider (subscription OAuth tokens and pasted API keys alike).

### `bitrouter auth login` / `logout` / `whoami`

Cloud sign-in, distinct from the per-provider `bitrouter login` flow above. Signs the CLI into your BitRouter Cloud account via the RFC 8628 OAuth Device Authorization Grant; the resulting bearer authenticates inbound calls to the cloud `/v1/*` surface (inference, key management, BYOK, policy, billing) and is auto-refreshed on use.

```
bitrouter auth login [--oauth-as <URL>] [--client-id <ID>] [--scope <SCOPE>]
bitrouter auth logout [--oauth-as <URL>] [--client-id <ID>]
bitrouter auth whoami
```

| Flag | Default | Description |
| --- | --- | --- |
| `--oauth-as` | `https://api.bitrouter.ai` (env: `BITROUTER_OAUTH_AS`) | Authorization server base URL — override only for a self-hosted deployment |
| `--client-id` | `bitrouter-cli` (env: `BITROUTER_OAUTH_CLIENT_ID`) | Public OAuth client id |
| `--scope` | broad developer set (env: `BITROUTER_OAUTH_SCOPE`) | Space-delimited scopes to request. Default includes `inference:invoke`, `usage:read`, `keys:read`/`write`, `billing:read`, `policy:read`/`write`, `byok:read`/`write`, `account:read`. Sensitive scopes (`billing:write`, `clients:read`/`write`, `account:write`) are opt-in. |

Credentials are persisted at `<data-dir>/account-credentials.json` (mode `0600` on Unix). `whoami` answers from the local file with no network call.

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

---

## Cloud account management

`bitrouter cloud …` drives the BitRouter Cloud `/v1/*` management surface (keys, usage, billing, policies, BYOK, OAuth clients) using the credential persisted by [`bitrouter auth login`](#bitrouter-auth-login--logout--whoami). Sign in first, then call any subcommand.

Every leaf accepts `--json` to print the raw response body instead of the human-readable summary. On a 403 whose description is `missing required scope: <s>`, the CLI prints a copy-pasteable re-login hint that appends the missing scope to your current set.

### `bitrouter cloud whoami`

```
bitrouter cloud whoami
```

Prints the cloud identity stored on this machine alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bitrouter cloud keys`

Manage `brk_` API keys on your account.

```
bitrouter cloud keys list [--json]
bitrouter cloud keys mint --name <NAME> --scope <SCOPE> [--scope <SCOPE> …] [--expires-at <RFC3339>] [--json]
bitrouter cloud keys revoke <ID> [--json]
```

Requested scopes on `mint` must be a subset of your effective scopes (RFC 6749 §3.3 — no upscaling). The plaintext token is shown once in the `mint` response and is not recoverable after.

### `bitrouter cloud usage` / `bitrouter cloud requests`

Read aggregate spend / token counts and page through recent inference requests.

```
bitrouter cloud usage    [--from <RFC3339>] [--to <RFC3339>] [--json]
bitrouter cloud requests [--limit <N>] [--offset <N>] [--json]
```

`usage` defaults to a 30-day rolling window. `requests` clamps the page size to `[1, 100]` and defaults to 25.

### `bitrouter cloud billing`

```
bitrouter cloud billing balance [--json]
bitrouter cloud billing checkout --amount-cents <N> [--json]
```

`checkout` starts a Stripe credit-purchase session and prints the hosted URL. Requires the `billing:write` scope, which is opt-in — pass `--scope` to `bitrouter auth login` to request it.

### `bitrouter cloud policy`

Generic CRUD over the typed policy registry (kinds: `budget`, `rate_limit`, `guardrail`, `preset`).

```
bitrouter cloud policy list [--kind <KIND>] [--json]
bitrouter cloud policy get <ID> [--json]
bitrouter cloud policy create --name <NAME> --kind <KIND> --spec <FILE|-> [--json]
bitrouter cloud policy update <ID> [--name <NAME>] [--spec <FILE|->] [--json]
bitrouter cloud policy delete <ID> [--json]
bitrouter cloud policy bind <ID> --principal-type <TYPE> --principal-id <ID> [--json]
bitrouter cloud policy unbind <ID> <BINDING_ID> [--json]
bitrouter cloud policy enable <ID> [--json]
bitrouter cloud policy disable <ID> [--json]
bitrouter cloud policy bindings <ID> [--json]
bitrouter cloud policy effective --principal-type <TYPE> --principal-id <ID> [--json]
bitrouter cloud policy for-principal <TYPE> <ID> [--json]
```

`--spec` reads the flat inner spec body as JSON from a file path or `-` for stdin. Principal types: `account`, `api_key`, `oauth_token`, `oauth_client`. `disable` parks a policy without deleting it — the engine skips disabled rows at request time.

### `bitrouter cloud budget` / `bitrouter cloud preset`

Typed wrappers over the budget-kind and preset-kind policy rows — same storage, flat wire shape (no `kind`/`spec` envelope).

```
bitrouter cloud budget list [--json]
bitrouter cloud budget get <ID> [--json]
bitrouter cloud budget create --name <NAME> --window <day|month|total> --limit-micro-usd <N> [--json]
bitrouter cloud budget update <ID> [--name <NAME>] [--window <W>] [--limit-micro-usd <N>] [--json]
bitrouter cloud budget delete <ID> [--json]

bitrouter cloud preset list [--json]
bitrouter cloud preset get <ID> [--json]
bitrouter cloud preset create --name <NAME> [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--json]
bitrouter cloud preset update <ID> [--name <NAME>] [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--clear-guardrail] [--clear-budget] [--clear-rate-limit] [--json]
bitrouter cloud preset delete <ID> [--json]
```

Budget `--limit-micro-usd` must be strictly positive (the engine treats `<= 0` as "no policy" and the API refuses it up-front). Preset clauses are independently optional; use `--clear-*` flags to drop a clause from an existing preset.

### `bitrouter cloud byok`

Manage bring-your-own-key provider keys. The cloud only stores already-sealed ciphertext — seal against the cloud's current X25519 public key (separate fetch) before calling.

```
bitrouter cloud byok list [--json]
bitrouter cloud byok set    --provider <ID> --ciphertext-b64 <B64> --kek-id <ID> --key-prefix <PREFIX> [--api-base <URL>] [--json]
bitrouter cloud byok delete <PROVIDER> [--json]
```

### `bitrouter cloud oauth-client`

Manage OAuth client registrations on your account. Requires `clients:read` / `clients:write`, both opt-in — request them via `bitrouter auth login --scope "<existing> clients:read clients:write"`.

```
bitrouter cloud oauth-client list [--json]
bitrouter cloud oauth-client register --name <NAME> --type <confidential|public> --grant <GRANT> [--grant <GRANT> …] [--scope <SCOPE> …] [--redirect-uri <URI> …] [--json]
bitrouter cloud oauth-client update <CLIENT_ID> [--name <NAME>] [--grant <GRANT> …] [--scope <SCOPE> …] [--redirect-uri <URI> …] [--json]
bitrouter cloud oauth-client delete <CLIENT_ID> [--json]
```

Grant types: `authorization_code`, `refresh_token`, `urn:ietf:params:oauth:grant-type:device_code`. For confidential clients, the freshly minted `client_secret` is returned exactly once in the `register` response.
