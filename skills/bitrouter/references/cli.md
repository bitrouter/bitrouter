# Gateway and BRO command reference

Run `bitrouter --help` on the service host and `bro --help` on the client. Each
product has its own installation receipt, data directory and update operation.
Use `bitrouter build-info` or `bro build-info` to identify the installed build.

| Operation | Entry point |
| --- | --- |
| Configure and start a gateway | `bitrouter init`, `bitrouter serve` |
| Inspect providers, routes, requests and service status | `bitrouter providers`, `bitrouter route`, `bitrouter requests`, `bitrouter status` |
| Manage model Policy, Eval and tenant configuration | `bitrouter policy`, `bitrouter eval`, `bitrouter tenancy` |
| Verify a native task after model completion | `bro task run PROMPT --workspace PATH --check COMMAND` |
| Run a native local task | `bro task run PROMPT --workspace PATH` |
| Inspect and operate a local thread | `bro thread list`, `bro thread history ID`, `bro thread pending ID`, `bro thread resume ID --turn TURN` |
| Connect an ACP manager over stdio | `bro acp serve AGENT` runs a catalog adapter with the gateway binding. `bro acp serve --agent NAME --direct --config agents.yaml` runs a configured local adapter. |
| Serve the native worker over ACP stdio | `bro acp serve` opens durable local sessions. Use `bro acp --workspace PATH serve --read-only` to restrict the workspace and tools. `--model MODEL` selects the gateway model. |
| Run an external ACP prompt | `bro acp list`, `bro acp --workspace PATH prompt --agent AGENT PROMPT` |
| Back up a stopped SQLite gateway database | `bitrouter tenancy offline snapshot`, `verify` and `restore` |

BRO reads the origin from `BITROUTER_ENDPOINT` and the client credential from
`BITROUTER_API_KEY`. Keep provider credentials in the gateway environment.
Approval, cancellation and resumption use the thread, turn and request IDs
emitted by BRO.

## Tenant configuration

These base-build commands use the HTTP `TenancyClient`. Select a named
`--context NAME`, or append `--endpoint URL --token-env NAME` to each remote
command. Never mix targets or pass a secret as an argument. No local daemon or
database is discovered; `--context local` is rejected. Server support for each
operation is required.

| Command | Meaning |
| --- | --- |
| `bitrouter tenancy capabilities` | Read the selected server's current tenancy descriptor. |
| `bitrouter tenancy configuration validate --input FILE` | Validate a full `ConfigurationPut` JSON file locally. No context/endpoint, authorization, resource resolution, compilation, or CAS checks. Reports `local_shape_only` and `server_checked: false`. |
| `bitrouter tenancy configuration apply --tenant ID --input FILE` | Prepare a draft using that same complete file. Does not activate it. |
| `bitrouter tenancy configuration read --installation-id ID --tenant ID --revision ID` | Read an exact retained revision. |
| `bitrouter tenancy configuration activate --tenant ID --input FILE` | Activate or roll back using a complete `Activation` JSON file and both observed CAS values. |
| `bitrouter tenancy configuration active --installation-id ID --tenant ID` | Read the active pointer and authority revision with tenant-wide read authority or an installation operator. |
| `bitrouter tenancy operation --installation-id ID --operation-id ID` | Recover the original principal's nonsecret result under current read authority. |

All remote rows above need an explicit target. Files are literal JSON (2 MiB
input, 1 MiB configuration limit), without environment expansion. Apply requires
`operation_id`, `installation_id`, `expected_authority_revision`, and `document`.
Activation also requires `expected_active_revision` (explicit `null` only when
none is active) and the exact `revision_id`. No identity/CAS value is generated
or refreshed; no mutation is automatically retried. An uncertain response retains
its original operation/installation for lookup. Do not replace its operation ID.
`operation` and activation reports with a non-`applied` status exit nonzero and
emit one object. JSON is the default; `--human` renders the same result.

Tenancy is disabled by default. The supported opt-in Unix owning-process profile
advertises `revision_bound_dispatch`, `strict_receipts` and `dispatch_enabled`
according to its current authority, storage and admission fences. Require all
three before strict inference. Administration success alone does not authorize
inference, and true flags do not reserve capacity or grant route access. Bind
the selected retained execution revision to the current descriptor and use an
issued tenant credential. See [remote administration](remote-administration.md)
for the supported profile, exact binding and uncertain-outcome recovery.

## Daemon lifecycle

For local resource authority, `tenancy.enabled: true` additionally requires a
dedicated `control:tenancy` credential. That scope is independent of host
read/reload grants. See [remote administration](remote-administration.md) for
the supported ownership profile and current dispatch eligibility; manage tenants
through `TenancyClient`, not host reload operation commands.

| Command | Effect |
|---|---|
| `bitrouter serve [--config PATH]` | Run the inference HTTP server + local control socket **in the foreground**. Optional `control.enabled: true` also starts the authenticated typed HTTP control listener at `control.listen` (default `127.0.0.1:4358`). `control.credentials` may name `{id, token_env, scopes}` credentials using `control:read` and `control:reload`; absent/empty credentials preserve `BITROUTER_CONTROL_TOKEN` as read-only. Explicit credentials exclude that legacy token. Tokens are at least 32 bytes, browser origins are checked, the listener stays loopback-only for a private tunnel/TLS reverse proxy, and inference `server.skip_auth` does not affect it. BRO serves ACP over local stdio. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Close admission, drain in-flight requests and start a new gateway process after shutdown. |
| `bro update [--check] [--tag TAG] [--stable] [-y]` | Update the local BRO executable from its own release archive and receipt. |
| `bitrouter update [--check] [--tag TAG] [--stable] [-y]` | Install the service executable and prompt for `bitrouter restart`. Homebrew, Cargo and npm installs use their owning package managers. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. `bitrouter --context NAME reload` instead reloads server-owned files without client env forwarding; non-success, running, partial, or unknown results exit non-zero and include a retained operation lookup. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. All three entrypoints reject startup-only changes before application; inspect `status.config_state` to choose reload or restart. |
| `bitrouter status [--config PATH] [--socket PATH]` | Passive status: pid / listen / model count / providers / socket, installed and daemon versions when available, and `spend`. Reports `stopped` (exit 0) when no daemon is reachable — and still reports spend, which is on disk and outlives the daemon. **`spend` is two independent facts**: `spend.spent` (money already gone — `estimated_micro_usd` over `window`, `requests`, `unpriced`) comes from the local metering database on *any* deployment; `spend.limit` (money left — `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd`) appears only where a cap exists, today a metered cloud account's prepaid credit. `spent` is an **estimate and a floor**: requests with no charge evidence are excluded rather than summed as zero, and `unpriced` counts them — non-zero means the figure understates by an unknown amount, so never read it as a total. A `limit` is the opposite, an authoritative ledger. No `spend` key at all means no metering database was readable, which is *not* the same as `estimated_micro_usd: 0`. Machine-wide, not per-caller. |
| `bitrouter requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]` | Newest-first settled requests (time, model, provider actually used, tokens, cost, latency, status) + the filtered-window spend and host-wide trailing-minute rate. **JSON by default**; `--human` renders the table. The time pair is required together and spans at most seven days; `limit` is 1–500. Reads the metering store directly and works with no daemon (`mode: history_only`). Portable to named remote contexts. `status --requests` is a hidden compatibility spelling. |

Configuration status uses the target-owned optional `config_state`: saved input availability, running `in_sync/reload_required/restart_required/mixed/unknown`, safe changed-field categories, and separate model-policy/access-policy evidence. Missing evidence from an older daemon stays unknown. Code inspectors consume this same report. Live responses never combine a client's alternate config with the daemon's source. A source-specific local locator preserves access to the running endpoint after socket edits or invalid/deleted YAML, and validates its daemon instance; `--socket` overrides it. `init` and `policy init` report `config_activation: saved_only`, not runtime activation.

Use the owning service manager to restart a foreground or shared gateway.
After restart, check the running version with `bitrouter status` for a local
daemon.

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--config PATH]` | Preview a model or router using the running daemon, otherwise local config (`resolved_via: live/config/zero_config`). The config fallback discovers models for `auto_discover: true` providers, as `bitrouter models` does. Fixed routes show `effective_model`, `provider_chain`, and available rate estimates. Policy-bound routers show identity, source, `bound_policy`, candidates, and `policy_decision_executed: false`; their `effective_model` is the base model and their provider chain is empty. No dynamic selection is executed. Nothing is sent upstream for inference; discovery may query provider `/models` endpoints. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model selector, each with **all** the providers that can serve it (the fallback chain, in order). Subscription providers are explicit-route-only and therefore appear as pinned `provider:canonical-model` selectors; every displayed selector can be passed unchanged to `bitrouter route`. Tries the running daemon first, falls back to a standalone config parse — same order as `bitrouter route`. The parse is resolved the way the daemon resolves its own config at start-up (built-in defaults, then subscription providers such as `claude-code` re-activated from the OAuth credential store), so a subscription-backed provider is listed with no daemon running; the live table additionally reflects `reload`s and whatever the daemon resolved at start-up. `--json` reports `resolved_via: "live" \| "config"`. Filter with `--provider`. |
| `bitrouter providers list [--config PATH] [--socket PATH]` | Local compatibility output retains `ID  MODELS  ACTIVE  API_BASE`; a named remote context and the dashboard use the redacted accepted catalog without API bases or credentials. Active means configured for routing rather than connectivity-probed. |
| `bitrouter providers check <provider> [--account LABEL] [--config PATH]` | GET the configured provider's model list with its protocol, configured API key or static headers, and upstream timeouts. Providers using stored logins need a configured key or static authentication header for the check. Local HTTP and online HTTPS share this check. Reports whether the endpoint answered and which model IDs it listed. Redirects return their original status. Unresolved credential references fail before a dial. One row per account unless `--account` selects one. Runs with local configuration and exits nonzero if a connection fails. |
| `bitrouter mcp list/show/add/edit/remove [--config PATH]` | Manage local upstream source entries without connecting; see [configuration](#mcp-configuration). |
| `bitrouter mcp check [server] [--config PATH]` | Connect to one configured upstream MCP server, or all of them, and report transport, reachability, latency, negotiated tools capability, and advertised tool names. |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: compiled/wired state, sampler, metric/cardinality counters, and in-flight spans. Local compatibility output retains socket, endpoint, and service fields; named remote contexts and the dashboard use the redacted report, which omits those host details and header values. |

## Durable trajectory operations

Enable `trajectory.enabled` in the gateway configuration and restart to capture
structural request and settlement evidence. This setting is off by default.
Use `bitrouter trajectory inspect EPISODE_ID` to read a recorded episode,
`bitrouter trajectory replay EPISODE_ID` to verify its history and
`bitrouter trajectory prune --before RFC3339 --dry-run` to preview retention.
These local commands accept `--config PATH`. Keep metering and outcome evidence
under their original identities.

## Generic eval exchange

These commands operate on the local append-only evidence ledger. Subjects,
results and frozen snapshots carry `spec_version: eval/1.0.0-beta.1`.

| Command | Effect |
|---|---|
| `bitrouter eval subject seal <DRAFT> --output <SEALED>` | Calculate the canonical digest of redacted evidence, validate the completed subject, and write deterministic JSON. This local file operation never opens the evidence ledger. |
| `bitrouter eval subject put <FILE> [--config PATH]` | Insert an immutable JSON/YAML request, episode, or task subject. |
| `bitrouter eval subject get <EVAL_ID> [--config PATH]` | Read one subject. |
| `bitrouter eval subject list [--config PATH]` | List subjects, including automatically observed routed requests. |
| `bitrouter eval result submit <FILE> [--config PATH]` | Submit a JSON/YAML evaluator result as the local operator; runs the same admission logic as REST. |
| `bitrouter eval snapshot freeze [--at RFC3339] [--config PATH]` | Freeze currently admitted results into a content-addressed manifest. |
| `bitrouter eval snapshot get <SHA256> [--config PATH]` | Read an immutable snapshot manifest. |
| `bitrouter eval status [--config PATH]` | Count subjects and latest admission states. |

Authenticated daemon endpoints mirror the exchange at
`/v1/evals/subjects`, `/v1/evals/results`, `/v1/evals/snapshots`, and
`/v1/evals/status`. External evaluators submit scores; BitRouter owns schema,
identity/metric authority, conflict/holdout admission, and snapshots.
The CLI uses the local ownership scope; authenticated REST is isolated by the
virtual key's owning user. Snapshot roots commit both subject and result content.

## Per-provider OAuth

| Command | Effect |
|---|---|
| `bitrouter providers login <provider>` | Per-provider OAuth. Supported providers include **`claude-code`**, **`github-copilot`**, and **`openai-codex`** — runs or adopts the provider's login flow and stores the refreshing token under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. |
| `bitrouter providers login <provider> --api-key <KEY>` / `--key-stdin` | Seed a BYOK provider (any that accepts a pasted key — `openai`, `anthropic`, `google`, `openrouter`, `opencode-*`) non-interactively: skips the method menu and the stdin paste. `--key-stdin` reads one line from stdin instead. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For `bitrouter`, the key seeds the cloud credential (same as `cloud login --api-key`). |
| `bitrouter providers logout <provider>` | Remove the stored OAuth token or credential for `<provider>`. |

## BitRouter Cloud sign-in (`bitrouter cloud …`)

OAuth 2.0 device-flow or non-interactive API-key sign-in against BitRouter Cloud. The persisted credential drives the raw API client, the `bitrouter` provider in the local daemon, telemetry attribution, and the management subcommands below.

| Command | Effect |
|---|---|
| `bitrouter cloud login [--oauth-as URL] [--client-id ID] [--scope SCOPE]` | RFC 8628 device-flow login. Prints an approval URL, polls the token endpoint, and persists access + refresh tokens to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Auto-refreshes within 60 s of access-token expiry on every subsequent call. Defaults: AS `https://api.bitrouter.ai`, client id `bitrouter-cli`, scope set covering `inference:invoke usage:read keys:* billing:read policy:* byok:* namespace:read`. |
| `bitrouter cloud login --api-key <BRK_API_KEY> [--oauth-as URL]` | Non-interactive CI login. Validates `brk_<token_id>.<secret>` and stores it without a network request. Conflicts with OAuth-only `--client-id` and `--scope`; never prints the key. |
| `bitrouter cloud logout` | OAuth: best-effort RFC 7009 revoke, then delete the local file. API key: local deletion only. |
| `bitrouter cloud whoami` | Print auth type (`oauth` or `api_key`) and non-secret local metadata. Reads the on-disk file only — no network. |

## BitRouter Cloud raw API (`bitrouter cloud api`)

`bitrouter cloud api <relative-endpoint>` mirrors the core `gh api` workflow and reuses either stored credential. It accepts arbitrary relative paths but never follows redirects or sends credentials off the login origin.

| Flag | Effect |
|---|---|
| `-X, --method METHOD` | Explicit method; implicit `GET`, or `POST` when fields/input are supplied. |
| `-H, --header KEY:VALUE` | Repeatable request header. User `Authorization` overrides the stored bearer. |
| `-f, --raw-field KEY=VALUE` | String JSON/query field with nested `key[sub]` / `key[]` grammar; bare `key[]` creates an empty array. |
| `-F, --field KEY=VALUE` | Typed bool/null/integer field, or `@file` / `@-` string content. |
| `--input FILE|-` | Exact request body; fields move to the query string. |
| `-i, --include` | Status line + response headers before body. |
| `--silent` | Drain without printing the body. |
| `--verbose` | Redacted method/URL/header/status diagnostics on stderr. |

Non-TTY JSON, binary bodies, and SSE stream byte-for-byte to stdout. HTTP 4xx/5xx preserves the body on stdout, writes the error to stderr, and exits non-zero. Tested endpoints include models, providers, public usage stats, Chat Completions, Messages, Responses, settlement receipts, routing presets, OAuth clients, billing ledger/status routes, and namespace/account management routes.

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bitrouter cloud …`)

Typed wrappers over the common Cloud management workflows. Requires either login form first. OAuth credentials use their baked namespace; API keys use `/v1/namespaces/me/*`. Every typed leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). Use `bitrouter cloud api <relative-endpoint>` for Cloud APIs that do not yet have typed CLI wrappers. On a 403 with `missing required scope: <s>`, OAuth users receive a copy-pasteable `--scope` re-login hint; API-key users are directed to a key with that scope.

| Command | Effect |
|---|---|
| `bitrouter cloud whoami` | Cloud base URL + local subject/scope from the credentials file. Offline. |
| `bitrouter cloud namespace list/current` | Inspect available workspaces and the one this CLI session is baked to. Lifecycle operations require the Console or raw `cloud api` with the right control-plane scope. |
| `bitrouter cloud keys list / mint / revoke` | List `brk_…` API keys, mint a new one (plaintext shown once), revoke by id. Scopes: `keys:read` / `keys:write`. |
| `bitrouter cloud usage [--from RFC3339] [--to RFC3339]` | Aggregate spend (micro-USD) + token counts over a window (default last 30 days). Scope: `usage:read`. |
| `bitrouter cloud requests [--limit N] [--offset N]` | Paged request history. Scope: `usage:read`. |
| `bitrouter cloud billing balance` | Credit balance + pending debits + available (`max(balance - pending, 0)`). Scope: `billing:read`. |
| `bitrouter cloud billing checkout --amount-cents N` | Start a Stripe checkout session for a credit top-up. Returns a hosted URL. Scope: `billing:write` (opt-in via `--scope` at login). |
| `bitrouter cloud policy list/get/create/update/delete/bind/unbind/disable/enable/bindings/effective/for-principal` | Generic CRUD over policy registry. `create` and `update --spec` accept a JSON file path or `-` for stdin. `effective` and `for-principal` answer "what would happen for this principal" without making an actual inference call. Scope: `policy:read` / `policy:write`. |
| `bitrouter cloud budget list/get/create/update/delete` | Typed sugar over budget-kind policies. |
| `bitrouter cloud preset list/get/create/update/delete` | Typed sugar over preset-kind policies. |
| `bitrouter cloud byok list/set/delete` | BYOK provider keys. `set` takes already-sealed ciphertext (`--ciphertext-b64` + `--kek-id` matching the cloud's current X25519 public key). Scope: `byok:read` / `byok:write`. |

## MCP configuration

`mcp list` reads configured servers; `show NAME` shows one. Reports name the absolute source, include each map key as id and its declared name, and redact headers/env, arguments and URL userinfo/query/fragment. Old aliases remain readable; writes require matching key/name.
`add NAME --url URL` and `add NAME --command PROGRAM -- ARGS...` save an entry.
`add/edit NAME --from FILE|-` accepts one complete YAML/JSON McpServerConfig: name, transport (http URL/headers or stdio command/args/env), aggregate and tool_prefix. Name must match the map key; --from conflicts with shortcuts. Edit replaces one whole entry; omitted fields use structural defaults. Add conflicts if the name exists; edit/remove require it.

Use `mcp edit NAME` with `$EDITOR` in a terminal; nonterminal edits need --from. Add can ask for transport/URL/program in a terminal. Prompts/editor use stderr and leave stdout JSON. Source changes while input/editor is open are rejected.

All commands honor --config and normal config precedence. Add can create a missing file at onboarding's explicit creation path; the receipt names it. Read/edit/remove never create an explicitly missing file. Ordinary block YAML preserves other nodes, comments, order and unexpanded environment references. Unsupported flow/anchor/complex layouts and link-backed write targets are refused with $EDITOR guidance. The shared publication lock protects the exact-source check through atomic replace, retaining permissions; arbitrary external writers must cooperate with that lock during publication. After a post-rename directory-sync failure, inspect saved state before retrying.

Saving validates a private candidate (including static MCP/host and policy rules); unset variables may use validation placeholders. It neither installs nor starts MCP processes, contacts upstreams, reloads nor restarts. Receipts say saved_only/running unknown and explain startup restart requirements. Use `bitrouter status` for target-owned evidence and explicitly restart after active work; route reload does not replace native MCP bindings. Check/tools/gateway/relay retain existing behavior. Hidden `mcp search QUERY` remains separate registry discovery. Versioned skill installation is not implemented by these commands.

Native tool errors and execution denials remain typed in the durable journal.
Model prompts represent them as JSON containing an explicit `status` and the
original typed `output`, so Chat Completions and Responses preserve the failure
meaning without a protocol-native error flag. Prompt byte limits include this
representation.


### Local DB-only offline evidence

On Unix, stop the gateway and all SQLite writers before taking a snapshot.
Use explicit local paths and a private scratch directory. The bundle path
must be new. Retain the digest printed by snapshot outside the bundle.

```sh
bitrouter tenancy offline snapshot --database /private/source.sqlite \
  --bundle /private/new-bundle --scratch-directory /private/scratch
bitrouter tenancy offline verify --bundle /private/new-bundle \
  --evidence-digest "$EVIDENCE_DIGEST" --scratch-directory /private/scratch
bitrouter tenancy offline restore --bundle /private/new-bundle \
  --evidence-digest "$EVIDENCE_DIGEST" --database /private/restored.sqlite \
  --expected-installation-id "$INSTALLATION_ID" --wrapping-key-env WRAPPING_KEY \
  --scratch-directory /private/scratch
```

Restore requires the original installation identity and wrapping key. Keep
the source database stopped while serving a restored copy. The snapshot
covers the gateway database. Preserve external credential files and other
deployment state separately.
