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
| Run an external ACP prompt | `bro acp list`, `bro acp --workspace PATH prompt --agent AGENT PROMPT` |
| Inspect a stopped M0 installation | `bitrouter tenancy offline split-store --database SOURCE --destination NEW_DIRECTORY` |

BRO reads the origin from `BITROUTER_ENDPOINT` and the client credential from
`BITROUTER_API_KEY`. Keep provider credentials in the gateway environment.
Approval, cancellation and resumption use the thread, turn and request IDs
emitted by BRO. A migration with `--activate` retires the source database path.
Keep its byte-exact rollback snapshot, configuration and external keys together.

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
| `bitrouter serve [--config PATH]` | Run the inference HTTP server + local control socket **in the foreground**. Optional `control.enabled: true` also starts the authenticated typed HTTP control listener at `control.listen` (default `127.0.0.1:4358`). `control.credentials` may name `{id, token_env, scopes}` credentials using `control:read` and `control:reload`; absent/empty credentials preserve `BITROUTER_CONTROL_TOKEN` as read-only. Explicit credentials exclude that legacy token. Tokens are at least 32 bytes, browser origins are checked, the listener stays loopback-only for a private tunnel/TLS reverse proxy, and inference `server.skip_auth` does not affect it. This optional HTTP control listener exposes neither an MCP origin nor remote ACP sessions; native ACP uses its separate OS-local stream. |
| `bitrouter start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bitrouter stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bitrouter restart [--config PATH] [--log PATH] [--socket PATH]` | Explicit restart: for file SQLite, preflight migrations on a snapshot and retain a recovery backup, then request graceful stop and start. Warns that active agent runs may be interrupted. Does not force-kill a process that fails to exit. |
| `bro update [--check] [--tag TAG] [--stable] [-y]` | Update the local BRO executable from its own release archive and receipt. |
| `bitrouter update [--check] [--tag TAG] [--stable] [-y]` | Self-managed service updates attempt a safe daemon handoff by default after replacing the binary; `--restart` is a hidden compatibility flag. Homebrew, Cargo, and npm installs delegate to their package managers. A busy or incompatible daemon is left running and reported as `deferred`. |
| `bitrouter reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bitrouter reload` rotates the key without a restart. `bitrouter --context NAME reload` instead reloads server-owned files without client env forwarding; non-success, running, partial, or unknown results exit non-zero and include a retained operation lookup. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. All three entrypoints reject startup-only changes before application; inspect `status.config_state` to choose reload or restart. |
| `bitrouter status [--config PATH] [--socket PATH]` | Passive status: pid / listen / model count / providers / socket, installed and daemon versions when available, and `spend`. Reports `stopped` (exit 0) when no daemon is reachable — and still reports spend, which is on disk and outlives the daemon. **`spend` is two independent facts**: `spend.spent` (money already gone — `estimated_micro_usd` over `window`, `requests`, `unpriced`) comes from the local metering database on *any* deployment; `spend.limit` (money left — `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd`) appears only where a cap exists, today a metered cloud account's prepaid credit. `spent` is an **estimate and a floor**: requests with no charge evidence are excluded rather than summed as zero, and `unpriced` counts them — non-zero means the figure understates by an unknown amount, so never read it as a total. A `limit` is the opposite, an authoritative ledger. No `spend` key at all means no metering database was readable, which is *not* the same as `estimated_micro_usd: 0`. Machine-wide, not per-caller. |
| `bitrouter requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]` | Newest-first settled requests (time, model, provider actually used, tokens, cost, latency, status) + the filtered-window spend and host-wide trailing-minute rate. **JSON by default**; `--human` renders the table. The time pair is required together and spans at most seven days; `limit` is 1–500. Reads the metering store directly and works with no daemon (`mode: history_only`). Portable to named remote contexts. `status --requests` is a hidden compatibility spelling. |

Configuration status uses the target-owned optional `config_state`: saved input availability, running `in_sync/reload_required/restart_required/mixed/unknown`, safe changed-field categories, and separate model-policy/access-policy evidence. Missing evidence from an older daemon stays unknown. Code inspectors consume this same report. Live responses never combine a client's alternate config with the daemon's source. A source-specific local locator preserves access to the running endpoint after socket edits or invalid/deleted YAML, and validates its daemon instance; `--socket` overrides it. `init` and `policy init` report `config_activation: saved_only`, not runtime activation.

The launcher passes the hidden `serve --managed-child` flag only to its own
detached child. Automatic handoff requires that ownership marker, an idle
daemon, and SQLite migration preflight. A foreground `bitrouter serve` stays under
its external supervisor's control.

Standalone BRO runtime daemons defer automatic replacement for the native
endpoint's lifetime; ACP/HTTP idleness cannot prove native queue/worker
quiescence. Finish native work and use explicit `bitrouter restart` to change binaries.

## Inspection

| Command | Effect |
|---|---|
| `bitrouter route <model> [--prompt TEXT] [--config PATH]` | Preview a model or router using the running daemon, otherwise local config (`resolved_via: live/config/zero_config`). The config fallback discovers models for `auto_discover: true` providers, as `bitrouter models` does. Fixed routes show `effective_model`, `provider_chain`, and available rate estimates. Policy-bound routers show identity, source, `bound_policy`, candidates, and `policy_decision_executed: false`; their `effective_model` is the base model and their provider chain is empty. No dynamic selection is executed. `--prompt` only affects the existing local static policy-table preview. Nothing is sent upstream for inference; discovery may query provider `/models` endpoints. |
| `bitrouter models [--config PATH] [--provider ID]` | List every routable model selector, each with **all** the providers that can serve it (the fallback chain, in order). Subscription providers are explicit-route-only and therefore appear as pinned `provider:canonical-model` selectors; every displayed selector can be passed unchanged to `bitrouter route`. Tries the running daemon first, falls back to a standalone config parse — same order as `bitrouter route`. The parse is resolved the way the daemon resolves its own config at start-up (built-in defaults, then subscription providers such as `claude-code` re-activated from the OAuth credential store), so a subscription-backed provider is listed with no daemon running; the live table additionally reflects `reload`s and whatever the daemon resolved at start-up. `--json` reports `resolved_via: "live" \| "config"`. Filter with `--provider`. |
| `bitrouter providers list [--config PATH] [--socket PATH]` | Local compatibility output retains `ID  MODELS  ACTIVE  API_BASE`; a named remote context and the dashboard use the redacted accepted catalog without API bases or credentials. Active means configured for routing rather than connectivity-probed. |
| `bitrouter providers check <provider> [--account LABEL] [--config PATH]` | GET the configured provider's model list with its protocol, configured API key or static headers, and upstream timeouts. Providers using stored logins need a configured key or static authentication header for the check. Local HTTP and online HTTPS share this check. Reports whether the endpoint answered and which model IDs it listed. Redirects return their original status. Unresolved credential references fail before a dial. One row per account unless `--account` selects one. Runs with local configuration and exits nonzero if a connection fails. |
| `bitrouter mcp list/show/add/edit/remove [--config PATH]` | Manage local upstream source entries without connecting; see [configuration](#mcp-configuration). |
| `bitrouter mcp check [server] [--config PATH]` | Connect to one configured upstream MCP server, or all of them, and report transport, reachability, latency, negotiated tools capability, and advertised tool names. |
| `bro agents list [--remote] [--config PATH] [--socket PATH]` | Show the accepted ACP catalog + which are configured. A named BitRouter context exposes this catalog only. Local `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bro agents inspect <agent> [--config PATH]` | Open a fresh ACP session, wait briefly for advertised slash commands, and report which source answers each command. |
| `bro agents check [agent] [--config PATH]` | Preflight one friendly/catalog agent or spawn each configured ACP agent and verify `initialize`. |
| `bro agents conformance <id>` | Run the `acp_compat_1` ACP-compatibility suite against a catalog agent and print the `conformance:` block to record in `registry/runtimes/<runtime>.yaml`. `<id>` is `<runtime>/<harness>` or a bare harness id (`local/` is the default runtime and may be elided). Two tiers: **handshake** (the agent answers `initialize` and settles on the ACP version its registry entry declares) and **routability** (the agent's LLM traffic reaches BitRouter when its routing block is applied). Needs **no provider credentials** — the agent is launched with its own routing pointed at an ephemeral loopback gateway that records what arrived; it does spawn the agent, so the package or binary must be installed. Exits non-zero when a tier fails, and prints no record in that case. |
| `bro agents scaffold <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). `agents install` is a hidden compatibility spelling. |
| `bro agents [--config PATH] [--socket PATH] [--limit N] [--after CURSOR]` | Read owner-local live instances: native workers, supervised ACP (including idle), native ACP attachments and managed Core channels. JSON/human, no TTY or implicit startup. `coverage` bounds observation; stopped uses `instances: null`, running observed zero uses `[]`, query failures error. Kind/ID bindings remain distinct; configured agents, historical runs and leases are excluded. Native `model_selector` is requested config; unconfirmed model/resources remain null. Page size is 1–16. Cursors are boot-scoped, sorted pages sample fresh membership. Subcommand target options follow the subcommand. |
| `bro agents sessions [--json] [--config PATH] [--socket PATH]` | List metadata-only supervised run summaries with typed lifecycle, attention, review, native-session identity, route/cost evidence, and attachment state. Its exact list grant cannot retrieve transcript, prompt/result, raw tool/permission context, or exact failure content. A settled turn is **Ready for review**, not “Completed.” |
| `bro agents show <agent-run-id>     # inspect retained state
bro agents events <agent-run-id>   # append retained/live events
| `bro agents stop <agent-run-id> [--config PATH] [--socket PATH]` | Stop and reap the controller/child without deleting the harness-native saved session. Idempotent cleanup preserves a prior failure. |
| `bro agents remove <agent-run-id> [--config PATH] [--socket PATH]` | Remove stopped, failed, or interrupted BitRouter metadata and ephemeral retained events after reaping settles. Does not delete native harness history. |
| `bitrouter observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: compiled/wired state, sampler, metric/cardinality counters, and in-flight spans. Local compatibility output retains socket, endpoint, and service fields; named remote contexts and the dashboard use the redacted report, which omits those host details and header values. |

## Durable trajectory operations

Trajectory capture is a local, explicit opt-in and is generic across tasks and
protocols. It never requires task datasets or private routing headers.

```yaml
trajectory:
  enabled: true
  retention_days: 30       # positive; default 30
  outbox_batch_size: 100   # 1..=1000; default 100
```

BitRouter fails closed if a signed lock contains any `progress_guard` while
`trajectory.enabled` is false. Every trajectory setting is restart-only:
reload rejects changes to `enabled`, `retention_days`, or `outbox_batch_size`
and preserves the live last-known-good state. Restart the daemon to apply any
of them. When enabled, capture uses the existing local correlation-key
lifecycle and startup retention. With `core` enabled, it also publishes the durable
evaluation outbox. Without core, gateway terminal facts and outbox envelopes stay
durable; publication attempts remain pending until a core-enabled daemon opens
the same state. Retention does not discard undelivered evidence. When trajectory
is disabled, it creates no correlation key and writes no trajectory ledger rows.

Provider-native Responses continuation remains always active even when
trajectory is disabled. Its independent lifecycle settings are:

```yaml
continuation:
  retention_days: 30       # positive; default 30
  prune_batch_size: 1000   # 1..=10000; default 1000
```

These settings are restart-only. The registry encrypts provider response IDs;
clients see only canonical `brc_` continuation IDs.

| Command | Effect |
|---|---|
| `bitrouter trajectory [--config <PATH>] inspect <EPISODE_ID>` | Resolve the globally unique episode, then report owner-scoped correlation/completeness, structural health, active hold, typed route clauses, and event digests. |
| `bitrouter trajectory [--config <PATH>] replay <EPISODE_ID>` | Audit a stable snapshot and compare the newest persisted route checkpoint digest with replay. Corruption output uses a stable reason code and the first intrinsically invalid event id/sequence only. |
| `bitrouter trajectory [--config <PATH>] prune --before <RFC3339> --dry-run` | Return exact global eligible counts without mutation. |
| `bitrouter trajectory [--config <PATH>] prune --before <RFC3339>` | Transactionally prune delivered old outbox rows and eligible terminal episode history in configured batches. |

`--config <PATH>` is optional and may appear before or after the trajectory leaf
command. Without it, all commands use the standard config resolution chain.
They open the selected source's database: a relative SQLite URL is anchored to
the config file's directory, or to the implicit BitRouter home (normally
`~/.bitrouter`) for zero-config. Resolution neither depends on nor changes the
caller's working directory; absolute/memory SQLite and server URLs are left
unchanged. The global `--json` / `--human` flags work before or after the
subcommand, and no operation takes an owner argument. Inspect/replay resolve the
owner once and keep all subsequent reads owner-filtered. Audit retries a bounded
head→events→head stable read, so a concurrent append yields a valid before/after
snapshot or a contention error, never false corruption. Its prefix reducer
distinguishes a valid route awaiting guard activation from intrinsic route or
guard corruption, so the reported first bad event is exact.

Retention uses an exclusive cutoff. An episode is eligible only when its last
capture is older, every request is settled or failed, and no request points at a
pending outbox row. Explicitly closed episodes must also have an old close time.
Deletion is owner/identity guarded and transactional; no synthetic close event
is written. Startup applies the same rules from `retention_days`. A late native
continuation after pruning becomes a new `unresolved` / `incomplete` episode.

Privacy is write-time, not display-time redaction: durable event JSON, outbox
payloads, Eval evidence, logs, and both report formats contain structural facts,
fixed categories/reason codes, counters, and digests only. They exclude
API/Bearer secrets, prompts, system instructions, tool arguments, file bodies,
and private provider metadata. Trajectory request IDs and Responses native-parent
IDs use the same installation-keyed, owner-bound opaque identity domain. This
does not change external request IDs carried on the wire, sent upstream, or used
for metering joins.

Built-in operational Eval results are always `inconclusive`: metrics describe
counts, streaks, elapsed time, and optional authoritative token/cost facts—not
task quality. Missing metering remains absent instead of becoming zero.
`trajectory.history_complete` is true only for a proven complete prefix;
incomplete/unknown history is not treated as complete and follows the guard's
configured incomplete-history behavior.

Recovery sequence: back up the configured DB → run prune `--dry-run` → inspect
valuable episodes → replay them → run real prune. Retry ordinary audit
contention after traffic quiets. Investigate stable corruption codes or restore
from backup.

## Generic eval exchange

These commands operate on the local append-only evidence ledger. They never
edit or publish `policy.yaml`.

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

With the application `core` feature, authenticated daemon endpoints mirror the exchange at
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

Snapshot, inventory and verify measure bounded SQL values and artifact
commitments. Their success does not authenticate every encrypted service payload
or establish runtime recovery eligibility. Audit, recovery and restore require
their own current-store and authority checks. Historical capacity-latch upgrades
and old Eval graph imports are retired; an artifact does not authorize rewriting
an old store into the new baseline.

`audit` and `recover` accept `--validation-page-rows` (default
32) and `--validation-bytes` (default 67108864). These bound temporary validation
memory, not total retained rows. Immutable bodies are checked sequentially under
the full working allowance. Increase working bytes after a capacity refusal;
lowering a budget does not make retained data corrupt. Validation can be
cancelled but never accepts an incomplete prefix. The existing DB-only bundle
and declared fresh-provisioning profiles retain their separate artifact limits.

Validation still needs enough workspace for one bounded object and the overlapping
evidence buffers. The current full evidence validator requires a page allowance
of at least 32 rows; this is a working-buffer minimum, not a total-row limit.
Treat insufficient working capacity as an incomplete validation, not permission
to discard records or clear a storage fence. These commands do not measure
throughput or establish a supported directory maximum.

Use `bitrouter tenancy offline snapshot --database FILE --bundle NEW_DIRECTORY`,
`bitrouter tenancy offline inventory --bundle DIRECTORY`, and
`bitrouter tenancy offline verify --bundle DIRECTORY --evidence-digest SHA256`.
Unix/file-SQLite only; paths are explicit, output must be new, and the source
must be stopped and exclusively owned. No daemon/config/HTTP discovery occurs.
`--context`, `--config`, endpoint and token flags are not accepted.

The secure0700/0600 bundle contains a DB snapshot, secret inventory key, nonsecret
evidence and READY marker. Save the returned digest separately. Never print or
share raw snapshot/key contents. Verification rechecks actual bounded typed SQL
values against the retained digest; it does not authorize restore or transfer.
Any INCOMPLETE marker makes the bundle unusable; do not delete it to force verify.

Results always say `complete:false`, `cutover_ready:false`: external OAuth files
and workspace state are excluded/unfenced, producer build/commit are unknown,
and this is neither a full backup nor an M0 migration manifest. Existing DB
sidecars fence participating writers only. Do not infer old OAuth writer custody.
Oversized/unsupported data fails rather than truncating inventory. No paid work,
owner assignment, migration, bootstrap or credential creation is performed.

Offline evidence rejects unsupported executable SQLite schema before integrity
checking or copying; ordinary BitRouter CHECK constraints remain supported.
Cancellation waits for driver cleanup under the canonical DB owner. If cleanup
cannot be confirmed, that process retains the source fence until exit. This does
not fence or include external OAuth/workspace state.

### Complete database evidence with bounded working memory

Snapshot defaults to the original v1 evidence profile. Select v2 explicitly for
complete retained data beyond that profile:

```sh
bitrouter tenancy offline snapshot --database /private/source.sqlite --bundle /private/new-bundle --format v2 --scratch-directory /private/scratch
bitrouter tenancy offline verify --bundle /private/new-bundle --evidence-digest SHA256 --scratch-directory /private/scratch
bitrouter tenancy offline inventory --bundle /private/new-bundle --scratch-directory /private/scratch
bitrouter tenancy offline restore --bundle /private/new-bundle --evidence-digest SHA256 --database /private/new.sqlite --expected-installation-id ID --wrapping-key-env TENANCY_KEY --scratch-directory /private/scratch
```

The scratch parent must already exist, be absolute and mode0700. Each command
creates its own temporary child. `--working-bytes` defaults to 67108864;
`--scratch-free-bytes` defaults to 268435456. These control working memory and
free-space reserves, not stored row counts or total database size. Individual
schema, SQL cell and typed-object bounds still apply. The existing
`--validation-bytes` spelling is an alias for the same working allowance;
supplying both spellings is rejected. `--validation-page-rows` defaults to 32
for authority validation. Low working capacity refuses the operation without
classifying valid retained data as corrupt.

The v2 source must be cleanly closed, with no WAL, SHM or rollback journal, and
all external writers fenced. The command does not delete journals to satisfy
this condition. Readers with explicit scratch accept supported v1/v2 evidence;
old v1-only readers refuse v2. Audit and recovery accept the same scratch flags;
v2 recovery plans use separately anchored streamed evidence. Offline mapping
remains its original bounded review profile. Historical authority upgrades and
Legacy Eval backfills have no CLI entry point; current backup/restore formats
remain supported.

Restore requires a new literal database path in a private directory and the
original wrapping key through its named environment variable. It preserves
the installation, operation/receipt history, identifiers and ciphertext. It
does not initialize, migrate, merge, start the service or release ingress.
Do not run the original and restored installation concurrently. A failed restore
can leave an INCOMPLETE staging directory; retain it for reconciliation and do
not retry onto an existing destination. Normal startup must still finish its
feature-specific validation, and external OAuth/workspace recovery remains a
separate obligation. Database evidence stays `complete=false` and
`cutover_ready=false`.

### Read-only retired receipt audit

`bitrouter tenancy offline audit --archive DIRECTORY --projection NEW_DIRECTORY --evidence-digest SHA256 --control-config FILE --listen 127.0.0.1:PORT` serves only the existing authenticated receipt GET/page routes for the archive's original installation. The explicit control file is bounded JSON with a `credentials` array of existing `id`, `token_env` and `scopes` entries; each requires `control:tenancy`. Bearer values are read only from those named environment variables, never command arguments. Example:

```json
{"credentials":[{"id":"archive-auditor","token_env":"ARCHIVE_CONTROL_TOKEN","scopes":["control:tenancy"]}]}
```

Use a private mode0600 control file and a mode0700 verified M4a bundle. The projection must be a new directory outside that bundle; its verified DB copy and canonical ownership sidecar do not alter the archive. Only Unix local SQLite with the exact supported current migration ledger and bounded schema/inventory is accepted. No source migration, missing-identity initialization, configuration loading, maintenance, credential refresh, provider call or command route runs. `--context`, endpoint/token flags and ambient host configuration are not audit inputs. The listener must be loopback; HTTP/2 follows the existing optional `http2` feature.

Every receipt request supplies `Authorization: Bearer ...` and the mandatory old `Bitrouter-Installation-Id`. Responses use `Cache-Control: no-store`. The first profile authenticates only the explicitly configured installation-operator control credentials; archived tenant ingress tokens are not audit credentials. Unknown receipt IDs are 404; wrong installation is a typed mismatch. Other management, inference, capability discovery and mutation routes are absent.

The running command emits one nonsecret readiness report containing the bound address, old installation and evidence digest. SIGINT/SIGTERM stops admission and retains each accepted read until its actual rollback and worker close are acknowledged. A dropped HTTP/CLI waiter cannot release the SQL owner. Unconfirmed bootstrap/close retains the canonical lease until process exit; elapsed time is not a cleanup witness. Limits: 32 sockets with a 60-second maximum connection lifetime, eight reads including queued/reply-held work, five-second read budget, up to 100 IDs/page and 8 MiB encoded response bytes. Reply buffers retain their read slot through transport clones and slices. Preparation observation is bounded to 120 seconds; cleanup observation is bounded to 30 seconds without aborting the owner.

The bundle remains `sqlite_database_only`, `complete=false`, `cutover_ready=false`. This command is receipt audit, not recovery verification, new-installation provisioning or a full backup. External credentials/workspace state and unknown upstream effects remain unassessed. Operators must maintain retirement of the source writers; a new sidecar does not fence arbitrary old binaries. Requalify corrected M4a and the final archive consumer before shipping this profile.

#### Offline mapping review

`bitrouter tenancy offline map --bundle DIRECTORY --evidence-digest SHA256 --manifest FILE --report NEW_DIRECTORY` validates the frozen M0 MigrationManifest and binds its source snapshot to a verified DB-only bundle. Inputs must be private regular files; no named context, host config or network lookup is used. This local profile accepts at most 8 MiB, 4096 mappings, 4096 merge approvals and 16 examples per mapping. Each declared disposition total must fit an exact JSON integer.

The new secured report directory records the original manifest and nonsecret validation summary. Duplicate mapping keys, missing or extraneous merge approvals, unresolved owners with grants, foreign observed installations and wrong snapshot digests fail. Same text in different owner namespaces remains distinct. The RECORDED marker only seals this review packet; it is never bundle READY or an execution capability.

Counts and evidence inside the manifest remain operator claims. The command does **not** authenticate external ownership/producer evidence, count per-owner source records, backfill rows, provision a target or retire writers. Missing installation identity remains explicitly unobserved. All completion/cutover flags stay false. This read-only packet does not enable historical data import or conversion. Provision a new installation through current APIs; restore requires a supported current dataset and its independent validation.

### Fresh installation recovery verification

`bitrouter tenancy offline recover --plan FILE --report NEW_DIRECTORY` verifies an explicitly retired archive and a stopped, normally Service-provisioned fresh target read-only. The private plan names the source evidence anchor, exact table dispositions, bound producer/retirement evidence, opaque selected files, new installation, all fresh operation/configuration readbacks, approved new static manifest and wrapping-key environment reference. See [the recovery profile](../../../docs/RESOURCE_TENANCY_RECOVERY.md) for exact fields, budgets and the operator fence boundary.

Use the normal Service/SDK to provision and read back authority/configuration first; this command does not issue credentials, import resources, copy history/keys, reset counters, start a server or release ingress. Source-equivalent definition transfer, OAuth/learned/paid state and automatic learning are unsupported. It checks actual retained authority, key epoch, authenticated static closure and exact fresh history. Keep the target stopped and the external writer/ingress fences maintained. A matching private `VERIFIED` marker with no `INCOMPLETE` records this narrow verification only; `complete`, `cutover_ready`, external-writer exclusion and upstream-effect resolution remain false. Existing report paths are never overwritten. Runtime qualification is separate from source delivery.
