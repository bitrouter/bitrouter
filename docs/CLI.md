# BitRouter CLI Reference

MCP proxy configuration and `mcp check`/legacy `tools` diagnostics require `mcp`.
Use `bitrouter` for the gateway daemon and operator commands. Use the independent
`bro` process for local workspace tools, thread checkpoints and ACP adapters.
The client requires the gateway origin and its own bearer credential.

SQLite needs no feature. PostgreSQL/MySQL configuration requires `db-postgres` /
`db-mysql`; both are included in the default distribution. Reduced builds report
an unavailable selected backend before connecting.
The opt-in `multi-instance` feature enables a separate shared foreground path for
`serve`: a literal `gateway_state.mode: shared` process document selects it before
local daemon/home discovery. It requires an initialized, published PostgreSQL
namespace and rejects managed-child/compiled-extension hosting. See the
[shared process configuration](../skills/bitrouter/references/shared-gateway.md).
`start` and `restart` reject shared configurations before creating local daemon
state or contacting an existing daemon. Shared services use foreground `serve`.
`bitrouter shared init --config PROCESS.yaml --plan SEED.json --operator-id OPERATOR
--operation-id ID [--global-principals]` explicitly initializes both database
schemas and atomically seeds a new namespace. The named PostgreSQL database and
deployment key must already exist. `--plan -` reads JSON from stdin. A seed is
limited to 2 MiB and 64 operations including its final publication; it uses the
configured transaction deadline. Principal creation requires the explicit global
flag because virtual principals are database-wide. This command does not use
named contexts, local daemon discovery or a serving incarnation. After an
uncertain response, retain the same actor, operation ID and identical plan;
replay returns the original receipt without overwriting later configuration.
See the [seed contract](../skills/bitrouter/references/shared-gateway.md#explicit-initialization).
Remote shared administration uses the separate operator listener and SDK:
`bitrouter shared apply --endpoint URL --token-env ENV --input MUTATION.json` submits
one literal JSON mutation. `--input -` reads stdin; input is limited to 2 MiB
and contains its caller-owned `operation_id`. No configuration or environment
substitution occurs inside the input. Endpoint and token are checked before
opening it. All remote shared commands require `multi-instance` and reject
named contexts before discovery or input reads.

`bitrouter shared operation --endpoint URL --token-env ENV --operation-id ID` reads
one actor-scoped receipt. A successfully read `unresolved` result exits zero;
it does not prove rollback and triggers no retry. The same target flags are
required for `revision`, `tenant --id ID`, `resource --id ID`,
`principal --id ID`, `credential --provider ID --account ID`, and
`execution --id ID`. These commands read metadata only and never export secrets.
After an unknown apply outcome, retain the original ID and private mutation and
query that ID with the same actor. A failed lookup cannot resolve the earlier
write. An output failure after a confirmed commit does not resubmit it.
Foreground startup reads a selected file once with a 16 MiB bound. Local
environment expansion is validated as local before daemon/home effects and the
same parsed input becomes its configuration baseline.
Gateway server-side tools require `server-tools` (included by default and enabling
`mcp`). Selecting MCP tools, advisor, subagent, Fusion, web search or web fetch
without that feature fails at startup, reload and profile preflight. Saved loop
tuning with no selected tool remains readable and does not activate execution.

Model-only Policy and the tenant Eval exchange are available in the minimal
gateway. Evaluation runs in a client or external evaluator. Submitting or
freezing evidence preserves the active Policy until an authorized activation.

## Executable identity

`bitrouter build-info` reports the service version, source revision, target,
effective features, profile and daemon handoff fingerprint. `bro build-info`
reports BRO's product, source revision, version, target and features. Both work
without a running gateway.

## Output format

Reporting commands print a single **formatted JSON object** to **stdout** for success or runtime failure, so output is machine-parseable by default (agent-native first). Argument parsing, help and version output follow the normal CLI parser channels. Global flags:

- `-j`, `--json` — force JSON (the default).
- `--human` — render a human-readable view to stdout instead of JSON.
- `--context <name>` — run a supported administration action against a named remote
  BitRouter target; `local` explicitly selects normal local behavior for commands
  that support it. `tenancy` HTTP commands require a named remote target.
- `-H` before the subcommand (for example, `bitrouter -H cloud whoami`) — compatibility spelling for `--human`. Under `bitrouter cloud api`, `-H` means `--header`, matching `gh api`.
- `-h`, `--help` — unchanged (`-h` is **not** human output).

Progress, warnings and diagnostic logs use **stderr** (colored when stderr is a TTY; honors `NO_COLOR`). Ordinary runtime failures are reported through stdout and can leave stderr empty. For reporting commands:

```
bitrouter <cmd> 2>/dev/null | jq .
```

yields one clean JSON value. A failed reporting command emits a uniform error envelope to stdout and exits non-zero:

```json
{ "error": { "kind": "not_found", "message": "…", "context": ["…"], "hint": "…" } }
```

Scripts inspect stdout's `error.message` and `error.context` for the failure and
its recovery details, including source restoration after a rejected publication.

`kind` is a stable taxonomy (`bad_request` / `unauthorized` / `forbidden` / `not_found` / `upstream` / `internal` / …). Under `--human`, the result (success object or error block) is rendered to stdout in the human form and no JSON is printed.

> Non-reporting commands are exempt: `serve` is a long-running server; `acp serve` is a stdio JSON-RPC bridge; foreground `run`, `task run`, `task managed`, and management `events --follow` emit NDJSON; `launch` inherits the terminal for the external harness; and `cloud api` streams the remote response body. Bare `agents`, `run --background`, and single-operation management commands remain ordinary structured reports.

## Local BRO threads and agents

BRO connects to the gateway origin selected by `--endpoint` or
`BITROUTER_ENDPOINT` and authenticates with `BITROUTER_API_KEY`. The default
origin is `http://127.0.0.1:4356`. Local checkpoints live in `BRO_HOME`
(default `~/.bro`) or the explicit `--data-dir`. Each thread records the
workspace and gateway at creation.

```sh
bro task run 'Summarize this project' --workspace . --read-only
bro thread list
bro thread history THREAD_ID
bro thread pending THREAD_ID
bro thread answer THREAD_ID --turn TURN_ID --request REQUEST_ID allow
bro thread resume THREAD_ID --turn TURN_ID
bro thread cancel THREAD_ID --turn TURN_ID
```

Use the IDs emitted by BRO. Native task output is NDJSON. An exact approval
authorizes a single recorded call. When an operation may have reached a model
or changed the workspace before its result was committed, recovery reports an
unresolved operation. A completed turn can be inspected after a process restart.

Use `bro acp list` to see bundled adapters and their headless gateway routing
support. For example, `bro acp --workspace . prompt --agent codex 'Inspect this
project'` runs an installed adapter with the selected gateway credentials.

## Logging (`RUST_LOG`)

Diagnostics are emitted with `tracing` and filtered by **`RUST_LOG`**, using standard [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax. When `RUST_LOG` is unset the filter defaults to `info`; a malformed value falls back to `info` rather than failing to start.

Most targets are Rust module paths (`bitrouter`, `bitrouter_sdk`, …), so `RUST_LOG=warn,bitrouter=debug` works as you would expect. Three targets are **pinned explicitly** and are *not* module paths:

| Target | What it carries |
| --- | --- |
| `bitrouter::observe::http` | DEBUG diagnostics from the HTTP ingress layer: one line per request, plus one when an inbound `traceparent` arrives but does not parse. **Silent unless OTel is configured** — the ingress layer is only installed when an exporter exists. |
| `bitrouter::observe::cardinality` | WARN when the metric-dimension cardinality limiter recovers from a poisoned lock. |
| `bitrouter::observe::span_attributes` | DEBUG per span attribute a deployment forwarded that the span schema reserves — see below. |

These use `::` separators (not `_`) precisely because they are not module paths: they are stable selectors that survive the code moving between crates. Two consequences for operators:

- **`RUST_LOG=bitrouter_observe=debug` selects nothing at all.** There is no `bitrouter-observe` crate; the exporter lives in application telemetry, so that is a dead selector rather than a narrower one — and the module-path fallback would be `bitrouter::telemetry::…`, which is exactly what the pins below exist to make irrelevant. The pinned `bitrouter::observe::*` targets below are the stable way to reach this instrumentation: use `RUST_LOG=bitrouter::observe::http=debug` (or a plain `info` default, which includes it).
- **Turn on `bitrouter::observe::span_attributes` when a forwarded attribute does not appear on a span.** A deployment can attach its own attributes to the root `chat` span, but the span schema reserves its own vocabulary: keys under `bitrouter.` or `gen_ai.`, and any key the schema already declares (`$screen_name`, `error.type`, `server.address`, …), are **dropped rather than stamped**, so one deployment cannot redefine what an attribute means for everyone else. The drop is deliberate and per-request, hence DEBUG rather than WARN: `RUST_LOG=info,bitrouter::observe::span_attributes=debug` names each dropped key. The full reserved region is `crates/bitrouter-ai/span-schema.json`.
- **`RUST_LOG` no longer affects tracing.** This used to be the opposite, and the reversal is worth stating because the old advice is still in circulation: the ingress span was a `tracing` span bridged into OpenTelemetry, so a filter that dropped `bitrouter::observe::http` at INFO also dropped the SERVER span, and every `chat` span exported as an orphan root with no error reported anywhere. A blanket `RUST_LOG=warn` was enough to do it. The ingress span is now an OpenTelemetry span in its own right and never passes through the `tracing` subscriber, so **no filter can suppress it**. Set `RUST_LOG` for the logs you want; traces are unaffected either way.

## Ignored configuration

`Config::plugins` is an unvalidated map and the JSON Schema declares it
`additionalProperties: true`, so a `plugins.<id>` block the binary does not
read is **silently ignored** — a typo like `plugins.bitrouter-guardrail`
(singular) drops the operator's declared block / redact patterns and the
process starts anyway. Two places report it:

- `bitrouter config validate` lists them under `ignored_config`. It does not
  fail validation — an ignored block is a misconfiguration, not a malformed
  config, and this command is CI-gating.
- Every runtime surface logs one WARN per unread id on start: the daemon, and
  `bro acp serve` and `bro run`, none of which build the
  daemon's `App` but all of which read the same config. This is the path that
  matters: validation is opt-in, the runtime always runs.

The supported ids the binary reads are `bitrouter-policy` and
`bitrouter-telemetry`. The removed `plugins.bitrouter-guardrails` key instead
fails validation and activation, including empty/null values; see
[the explicit migration guide](GUARDRAILS_EXTENSION.md). A dead sub-key under a live id is reported too, so a
rename that carries an obsolete setting along with it is not silent either.

**Renamed in this release** — the old names are ignored, and the daemon warns
when it sees one set:

| Old | New |
| --- | --- |
| `plugins.bitrouter-observe.*` | `plugins.bitrouter-telemetry.*` |
| `BITROUTER_OBSERVE_CONTENT_CAPTURE` | `BITROUTER_TELEMETRY_CONTENT_CAPTURE` |
| `BITROUTER_OBSERVE_CONTENT_ATTR_MAX_BYTES` | `BITROUTER_TELEMETRY_CONTENT_ATTR_MAX_BYTES` |

`plugins.bitrouter-observe.otlp_endpoint`, the v0 flat shim, is **removed**
rather than carried over: it existed to keep a v0 config building, and v0 never
had a `plugins.bitrouter-telemetry` key for it to live under.

The `bitrouter::observe::*` log targets above, the `io.bitrouter.observe`
instrumentation scope, and the `bitrouter` meter name are **not** renamed and
will not be. They are wire and `RUST_LOG` contract — a rename there is silently
wrong for every dashboard and every existing selector, with no safety net
possible.

## Config resolution

Local router subcommands that load a config accept an optional `-c / --config <path>` flag. When omitted the binary walks this order:

1. `./bitrouter.yaml` in the current directory
2. `$BITROUTER_HOME/bitrouter.yaml` — if the env var is set, the file must exist
3. `~/.bitrouter/bitrouter.yaml` — used if present
4. **Zero-config** — in-memory defaults; auto-enables any provider whose API key is set in the environment

Daemon-control subcommands (`stop`, `reload`, `status`) also accept `--socket <path>` to override the control socket path derived from the config.

## Remote contexts

```console
bitrouter context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_BITROUTER_TOKEN
bitrouter context list
bitrouter context show workstation
bitrouter --context workstation status
bitrouter --context workstation requests
bitrouter --context workstation models --provider openai
bitrouter --context workstation route openai/gpt-5
bitrouter --context workstation providers list
bitrouter --context workstation observe status
bitrouter --context workstation policy status       # active by default
bitrouter --context workstation policy show default --view disk
bitrouter --context workstation agents list
bitrouter --context workstation reload
bitrouter context remove workstation
```

Contexts live in `contexts.toml` under `$BITROUTER_HOME`, or under
`~/.bitrouter` when that variable is unset. They contain the endpoint and token
environment-variable **name**, never the bearer value. Endpoints must use HTTPS;
plain HTTP is accepted only for `localhost`/loopback, including an SSH-forwarded
port. Host administration performs the `/control/v1/capabilities` handshake before its
first action (and retains it for that command), follows no redirects, and
never falls back to this computer's config, socket, or database after a remote
error. A named context is the complete target: supported remote commands reject
local `--config` and `--socket` flags before opening local configuration,
environment, or metering state.

Remote read actions are `status`, `requests`, `models`, `route`, `providers
list`, `observe status`, `policy status`, `policy show`, and `agents list`.
Policy reads default to `--view active` for both local and remote targets.
Select `--disk` or `--view disk` explicitly for saved source, including offline
local inspection. An unavailable active instance never silently becomes a disk
read. `policy show [NAME]` returns the complete resubmittable `source` document;
view, availability, `source_digest`, names and bindings stay in the outer report.
The control handshake requires protocol 2 and compatible action descriptors;
Policy show/status use descriptor version 2. Unsupported peers are rejected
before an action, without legacy action-name fallback.

`agents list` is a catalog read only. `agents list --remote`, agent checks and
launches, ACP sessions, and native execution commands stay local. The
remote host mutation is the explicit `reload` action; it reloads server-owned
files and never uploads config or forwards the client's environment. The separate
`tenancy` command group prepares and activates typed tenant configuration through
the tenancy HTTP SDK. Host provider, key, configuration, policy-publication, and
service lifecycle administration remain local.

Local CLI compatibility adapters retain the established `providers list`
`api_base` field and `observe status` socket, endpoint, and service fields.
Those host details are omitted from remote administration and dashboard
reports, which use the redacted typed read contracts.

## Tenant configuration over HTTP

The HTTP subcommands of `bitrouter tenancy` are available in the base gateway build.
They are client adapters;
the server must implement the selected tenancy operation. Choose either a named
context or an explicit `--endpoint URL --token-env NAME` pair on each remote
command. Mixing them is an error. There is no default target, local daemon
discovery, database access, or `--context local` fallback. The token comes only
from the selected environment variable; no token-value flag is provided. HTTPS
or loopback HTTP is required. The client follows no redirects and preserves the
existing CLI system-proxy behavior.

```console
bitrouter context add resource-admin \
  --endpoint https://router.example/control/v1 --token-env RESOURCE_ADMIN_TOKEN
bitrouter --context resource-admin tenancy capabilities
bitrouter tenancy capabilities --endpoint http://127.0.0.1:4358 --token-env RESOURCE_ADMIN_TOKEN
bitrouter tenancy configuration validate --input draft.json
bitrouter --context resource-admin tenancy configuration apply --tenant tenant-a --input draft.json
bitrouter --context resource-admin tenancy configuration read \
  --installation-id install-a --tenant tenant-a --revision config-r1
bitrouter --context resource-admin tenancy configuration active \
  --installation-id install-a --tenant tenant-a
bitrouter --context resource-admin tenancy configuration activate --tenant tenant-a --input activation.json
bitrouter --context resource-admin tenancy operation --installation-id install-a --operation-id prepare-1
```

| Command | Input and effect |
| --- | --- |
| `tenancy capabilities` | One fresh `TenantCapabilities` read. No host-capabilities handshake or implicit submission follows. |
| `tenancy configuration validate --input FILE` | Local `ConfigurationPut` JSON shape, duplicate/unknown-field, reference-count and numeric-bound checks only. Rejects target/context flags. No authorization, resource lookup, compilation, CAS or dispatch check. |
| `tenancy configuration apply --tenant ID --input FILE` | One `put_configuration` call. Prepares an immutable draft; does not activate it. |
| `tenancy configuration read --installation-id ID --tenant ID --revision ID` | Reads exactly the requested retained revision under current grants. |
| `tenancy configuration activate --tenant ID --input FILE` | One `activate` call with the complete `Activation` document; rollback uses the same command with the chosen retained revision. |
| `tenancy configuration active --installation-id ID --tenant ID` | Reads the current pointer and authority revision; does not copy them into any submission. Requires tenant-wide read authority or an installation operator. |
| `tenancy operation --installation-id ID --operation-id ID` | Reads the original principal's nonsecret management result under current read authority. Separate from host `operations show`. |

Files are literal UTF-8 JSON, bounded to 2 MiB; the configuration document also
has the contract's 1 MiB bound. No YAML, environment expansion, host-file upload,
or implicit resource import occurs. Validate and apply take the **same complete
ConfigurationPut file**, for example:

```json
{
  "operation_id": "prepare-1",
  "installation_id": "install-a",
  "expected_authority_revision": 17,
  "document": {
    "routes": [
      {
        "owner_kind": "tenant",
        "owner_id": "tenant-a",
        "resource_kind": "route",
        "resource_id": "coding",
        "revision_id": "route-r1"
      }
    ],
    "policy": null,
    "generation_defaults": { "max_output_tokens": 2048, "temperature": 0.2 },
    "learning": { "mode": "off", "max_evaluations": 0 },
    "limits": { "active_executions": 16, "queued_work": 32, "learning_jobs": 0 }
  }
}
```

These IDs and revisions are illustrative. Use the actual installation, observed
authority revision, and already provisioned owner-qualified resource revisions.
The local validator can accept references that the server cannot resolve or
authorize. Its output explicitly reports `validation: "local_shape_only"` and
`server_checked: false`.

`activation.json` supplies both CAS observations and the exact retained target:

```json
{
  "operation_id": "activate-1",
  "installation_id": "install-a",
  "expected_authority_revision": 18,
  "expected_active_revision": null,
  "revision_id": "config-r1"
}
```

`expected_active_revision` must be present. Use `null` only after observing no
active revision; otherwise supply the observed ID. The CLI never generates an
operation ID, fetches a replacement CAS value, substitutes an installation, or
retries a mutation. After an unknown response, retain the exact request and query
its original operation with the original installation and same principal's
current read authority. A missing result does not prove non-application. An
applied configuration operation identifies the revision for an explicit read.
Do not submit a new operation ID to resolve uncertainty.

Output uses the usual JSON default and `--human` view. Remote success output keeps
the typed service response shape; operation statuses other than `applied` exit
nonzero while emitting one operation object. Capability discovery remains a
successful read when dispatch is disabled. The presence of these commands, local
validation, or a prepared/activated revision does not establish inference support:
`revision_bound_dispatch`, `strict_receipts`, and `dispatch_enabled` must all be
true for strict tenant dispatch. Authority-only servers keep these flags false.

---

## Daemon lifecycle

### `bitrouter serve`

Run the HTTP server and control socket **in the foreground**.

```
bitrouter serve [-c <path>]
```

Starts the proxy on the configured listen address (default `127.0.0.1:4356`) and opens a Unix domain control socket. Logs to stdout.

An opt-in remote-control listener exposes typed administration reads and a
separately authorized reload action to a trusted operator through a private
tunnel or TLS reverse proxy:

```yaml
control:
  enabled: true
  listen: 127.0.0.1:4358
  credentials:
    - id: workstation-admin
      token_env: WORKSTATION_ADMIN_CONTROL_TOKEN
      scopes: [control:read, control:reload]
```

`ControlConfig.credentials` is optional. When it is absent or empty, the legacy
`BITROUTER_CONTROL_TOKEN` remains valid with `control:read` only. When explicit
credentials are configured, only their `id`, `token_env`, and `scopes` entries
are accepted; the legacy token is not an additional credential. Each token must
be at least 32 bytes. `control:read` grants the host-wide operational reports;
`control:reload` permits the guarded reload operation and requires the read
scope alongside it.

`control:tenancy` grants installation resource management independently of host
read/reload authority. The opt-in `tenancy.enabled` service uses this credential
on the same protected listener. It currently requires a Unix host, local gateway
state, authenticated ingress and a literal SQLite file URL. Tenancy configuration
is restart-only. Use `bitrouter-sdk`'s `TenancyClient` for these operations;
host reload operation IDs belong to a separate namespace. This stage exposes
authority and retained-configuration management with tenant dispatch disabled
pending runtime execution binding and strict receipts.

The optional restart-only `tenancy.resource_import` names an operator JSON
manifest, relative to the server process working directory when not absolute.
Set `tenancy.wrapping_key_env` to the name of an environment variable containing
a stable base64-encoded 32-byte wrapping key before importing resources. These
settings require `tenancy.enabled`; import additionally requires the key setting.
The key is also required on subsequent restarts while retained resources exist,
even if the manifest path has been removed. A missing/wrong key fails startup.
No key is generated implicitly, and changing it is not a key-rotation operation.

The manifest's format, `installation_id`, and `resources` identify an explicit
installation catalog. Use the format discriminator required by its resource
kinds; a format number is not an instruction to migrate existing data. Each
resource contains its qualified `reference` and typed `definition`. Routes retain
their routing source, literal selector and explicit provider/account credential
references. Routing Policies retain the public model-only `policy::Document`
and its exact selected name. Context actions and experimental route members
cannot obtain mature service execution authority. The initial importer accepts
static API-key credentials with generation zero; OAuth/ambient stores
and native request-check definitions remain unsupported. The manifest is
privileged secret-bearing input: tenant configuration requests only name
references and cannot supply provider endpoints, keys, files or executables.

Imports commit atomically before listeners start. Identical qualified revisions
are idempotent; changed content under the same revision is rejected. Removing or
editing a source file does not alter retained revisions. Catalog definitions and
compiled configurations are authenticated ciphertext; public configuration
responses expose references and the public document only. Active compiled
configurations are verified again on restart and before activation. Import files
are limited to 4 MiB/256 resources, with at most 4,096 retained resources; these
are internal provisioning bounds. Tenant draft documents retain their public
1 MiB/256-reference limits.

An operator's principal ID is `operator.<configured credential ID>`, preserving
case. Tenancy credential IDs are limited to 119 bytes. Use the public
`PrincipalId::for_control_credential` helper as `expected_management_owner` when
updating a binding created by that operator. Changing the token or `token_env`
preserves identity; renaming the ID changes it. Reusing an old ID intentionally
reuses its management and operation identity.

Stop existing writers before enabling tenancy on a legacy SQLite file, including
processes from releases that predate file ownership leases. Current writable
pools hold a shared lease; an authority owner requires exclusive access and
retains it through shutdown and outstanding transactions. `mode=rw` requires an
existing file. Host-authorized read-only reports remain available; direct local
commands cannot reopen a provisioned authority store for writing.

This listener is disabled by default, accepts loopback addresses only, and is
never affected by inference `server.skip_auth`. Its typed HTTP actions live
under `/control/v1`; it exposes neither an MCP origin nor remote ACP sessions.
Changes under `control:` are restart-only because they govern listener creation
and binding.

### `bitrouter start`

Spawn `serve` as a **detached background daemon**.

```
bitrouter start [-c <path>] [--log <path>]
```

Logs default to `bitrouter.log` next to the config file (e.g. `~/.bitrouter/bitrouter.log` when the config resolved to `~/.bitrouter/bitrouter.yaml`). Refuses to start if a daemon is already running.

Waits until the daemon answers on its control socket before reporting `✓ … started` (up to 15s), then prints the listen address and routable-model count — so a follow-up command can rely on the daemon being up. If the daemon crashes during startup, the tail of its log is printed and the command exits non-zero; if it is alive but still not ready after 15s, a note is printed and the command exits 0 (the daemon keeps coming up).

### `bitrouter stop`

```
bitrouter stop [-c <path>] [--socket <path>]
```

### `bitrouter restart`

```
bitrouter restart [-c <path>] [--socket <path>] [--log <path>]
```

Stops the running daemon (waiting up to 30s for in-flight requests to drain), then starts a fresh one.
For a file SQLite database, restart first validates migration lineage and runs
pending migrations on a private snapshot. It retains a recovery backup before
stopping the daemon. The explicit restart may interrupt active agent runs.
When both executables report different compiled distributions, restart instead
requires verified launcher ownership and the atomic idle handoff gate before
stopping anything. Unsupported configuration, active work or unknown idleness
leaves the resident unchanged. The same-profile explicit restart retains its
existing interruption behavior.

A profile transition can also apply a saved control-socket change. Preflight
checks the candidate endpoint before stopping the resident, and readiness,
artifact identity and locator verification use that new endpoint.

### Local daemon after an update

`bitrouter status` reports the installed and running daemon versions when the daemon
supports that metadata. Its JSON also includes `compatibility`, launcher
ownership, and observed active-work counts when the daemon can report them.
`installed_artifact` and `daemon_artifact` identify the corresponding compiled
distributions. Missing legacy metadata remains unknown. A `profile_mismatch`
allows common inspection but prevents automatic replacement by a different
feature set or target; use a matching executable for execution or request an
explicit profile transition. Control HTTP capabilities also report the server's
artifact separately from the caller's authorized actions.
These counts are a passive snapshot; the daemon checks them again behind its
drain gate before stopping. A new `bro` probes the local daemon before supervised
session commands. When the daemon has the handoff protocol, is idle, and its
SQLite migration preflight succeeds, `bro` starts the installed binary through
a graceful daemon handoff. Active agent runs, ACP recordings or route leases,
and in-flight HTTP responses defer the handoff. A legacy daemon cannot prove
it is idle; finish its work and run `bitrouter restart` explicitly.
Foreground `bitrouter serve` remains under its external supervisor; the CLI does not
take it over automatically.

Every `core` build holds daemon admission for its managed runtime's lifetime,
including core-only sessions executed by an external harness. Channel work,
pending checkpoint acknowledgements, native requests and queue runners do not
yet participate in an atomic handoff, so even an apparently idle core resident
defers automatic replacement. Gateway and harness-only builds retain tracked
idle handoff. Finish managed work and use the explicit `bitrouter restart` when
changing binaries.
That exception applies to the same compiled distribution. A cross-profile
restart still requires atomic idle evidence and is refused by the managed
lifetime guard; plan an explicit stop after finishing work before starting the
other profile. No command silently removes that guard to change profiles.

`bro update` attempts the same safe handoff after a self-managed installation.
Package-manager installs still delegate the binary update; the next
daemon-dependent invocation checks compatibility. `bro update --check` and
`bitrouter status` do not restart the daemon. `bro run AGENT PROMPT --no-start` will diagnose a
version mismatch without performing the handoff. The former `--restart` flag
is accepted for compatibility; safe handoff is now the default.

A native `checkpoint capacity exhausted` outcome is a storage-capacity boundary.
Its cleanup settles outstanding work before termination. After a stopped owner
and complete terminal journal are confirmed, fresh workspace work is allowed;
a prepared request rejected before dispatch does not count as a model attempt.
Unconfirmed tool effects and incomplete records still require recovery.

Native execution journals compress checkpoint content without discarding
history. Startup upgrades older compressed rows in the ownership transaction,
preserving each original record's bytes, hash and cursor. The updated binary
reads old journals, but older binaries cannot read the new row encoding.

### `bitrouter reload`

```
bitrouter reload [-c <path>] [--socket <path>]
bitrouter --context <name> reload
```

Hot-reloads supported configuration participants without dropping connections. Local IPC, `SIGHUP`, and remote reload use the same restart-required classification: changes to startup-owned settings (such as listen/control, plugins, or named routers) are rejected before any live participant changes. Restart the daemon to apply them.

Provider API keys in the current local environment are forwarded into reload preparation. `export OPENAI_API_KEY=…; bitrouter reload` applies supported credential changes when the coordinated reload succeeds; startup-owned changes still require restart.

With `--context`, reload submits a guarded operation against the server's own
current configuration. It does not forward client environment variables. The
report includes a request id, server instance, generation, and per-subsystem
result. A running, failed, partially applied, or unknown operation exits
non-zero; use its operation lookup details and the target's state before retrying.

### `bitrouter status`

```
bitrouter status [-c <path>] [--socket <path>]
bitrouter requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]
bitrouter requests --human           # the same, as a table
```

Prints pid, listen address, number of routable models, the distinct providers behind them, the control socket path, and the **spend position**. Exits cleanly with "stopped" when no daemon is reachable.

The same app-owned report used by CLI clients and typed remote control, so
each retained surface preserves one JSON shape.

**Configuration state.** `config_state` is an optional, target-owned report shared by local status, remote control, and Code inspectors. It includes:

| Field | Meaning |
|---|---|
| `source` / `saved` | file or generated defaults; available, generated, missing, invalid, or unavailable saved input |
| `running` | `in_sync`, `reload_required`, `restart_required`, `mixed`, or `unknown` for the inspected configuration sources |
| `reload_required_fields` / `restart_required_fields` | safe field categories that differ; no configuration values or secret-derived digests |
| `named_policy` / `access_policies` | separate evidence for model-policy source and access-policy files; unchanged YAML alone does not establish synchronization |
| `server_instance_id` / `generation` / `last_reload` | serving process and coordinated reload evidence, when available |
| `mixed_state_history` | retained partial/interrupted outcomes that explain a mixed runtime, even after a later preparation failure |

An active daemon supplies both saved and running evidence from its own configuration source. An alternate client `--config` cannot replace the saved half of another socket's reply. A stopped daemon can expose saved-only evidence, with running state unknown. An older daemon that omits the field remains unknown. Invalid or missing saved configuration does not erase a reachable runtime.

If a daemon started with generated defaults and a `bitrouter.yaml` is then created in its home, `source` still identifies the running default source while `saved` describes the new file. `restart_required_fields: ["configuration_source"]` requires restart to adopt it; reload cannot silently ignore the file or switch sources.

For local administration, the daemon records its actual endpoint beside the configuration under a private, source-specific locator. Status validates the process instance before using it, so editing the saved control socket or damaging/deleting YAML does not make the still-running daemon disappear. An explicit `--socket` remains authoritative. Remote clients use only the selected server's evidence.

`in_sync` describes inspected configuration inputs; it is not a provider connectivity probe or a claim that external OAuth/registry services have not changed. Initialization and policy initialization report `config_activation: saved_only`: saving files does not prove a running daemon applied them. Inspect status, then reload or restart as indicated.

**`spend` — what has gone, and what is left.** Two independent facts, each present only where the deployment can answer it:

| Key | Filled by | Means |
|---|---|---|
| `spend.spent` | any deployment (the local metering database) | Money already gone: `estimated_micro_usd` over `window` (today, since 00:00 UTC), `requests`, and `unpriced` |
| `spend.limit` | a deployment with a cap (today: a metered cloud account's prepaid credit) | Money still available: `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd` |

`spent` is an **estimate and a floor**, not a total. It is priced from BitRouter's own registry at settle time, and requests with no charge evidence are excluded rather than summed as zero — summing them would report a floor as a price. `unpriced` counts exactly those, so a non-zero value means the figure understates by an unknown amount; the human view marks it `floor, not a total`. A `spend.limit` is the opposite kind of number: an authoritative ledger the account is settled against. Read `unpriced` before treating the two as comparable.

The read is best-effort and never fails the command: no config, no database file, or an unreadable one gives no `spend` key at all — which is a different answer from `estimated_micro_usd: 0` over `0` requests, meaning "nothing spent today". It also works with **no daemon running**, so a `stopped` report still carries spend: what a past daemon spent is on disk and does not stop being true when it exits.

The figure is **machine-wide**, not per-caller: it rolls up every caller of this daemon, the same scope `requests` reports. Per-session spend is reported through attributed ACP usage and retained run events.

`bitrouter status --json` gained `spend` additively; every pre-existing key is unchanged.

`bitrouter requests` reports what the router has actually done: newest-first
settled requests — time, model, the provider that **actually** served, tokens
in/out, cost, latency, status — plus daemon state and the window's spend and
trailing-minute rate. It reads the metering store directly, so it also works
with **no daemon running**. `--since` and `--until` are an RFC3339 pair with a
maximum seven-day interval; absent both, the server selects today from UTC
midnight. `--model` and `--provider` filter the same report before the limit.
`status --requests` remains a hidden compatibility spelling.

Like every other report it uses JSON by default and `--human` for the table. Repeat it with `watch -n1 bitrouter requests --human` for a live view.

The spend rollup covers every caller of the daemon, not one session. Attributed ACP usage and retained run events provide the per-session figure.

**Spend is reported only where there is evidence.** An unpriced request shows `?`, not `$0.00`; the same honesty rule applies to `code <agent>` session cost.

Each row also carries `episode_id` — the trajectory episode to hand to `bitrouter trajectory inspect`, or `null` when trajectory capture recorded nothing for it (capture is opt-in and off by default, so `null` is the common case). It is the thread from a settled request to its structural record, which is otherwise reachable only by an episode id nothing else hands out.

Portable — there is no terminal-only path left to gate.

> **`--requests` emits JSON by default as of 1.0.0-alpha.28.** It previously printed the table unconditionally, ignoring `--json` — the only `status` path that did. Scripts that parsed the table need `--human`; anything that wanted the data now gets one clean JSON object with a stable `rows[]`.
>
> **Replaces `--watch` (`-w`), removed in 1.0.0-alpha.28.** That flag opened a self-refreshing ratatui view with cursor keys plus `r` (reload) and `e` (`$EDITOR` on `bitrouter.yaml`). Both of those keys ran commands you can still run directly — `bitrouter reload`, and your editor — and the piped form of `--watch` printed what `--requests --human` prints now.

---

## Config

### `bitrouter config validate`

```bash
bitrouter config validate --config bitrouter.yaml
```

Validation runs the real configuration parser, provider inheritance, upstream
URL checks and referenced model-policy source validation. It reports named-router
counts and exits nonzero for invalid input. Unset environment references appear
as warnings using validation placeholders; recheck those values in the actual
runtime environment before activation.

Define named routers under `routers` and address them as `bitrouter/<id>`.
A router explicitly selects a fixed model or a literal named policy and base
model. Old preset/variant configuration, compatibility router addresses and
Policy mode fields are rejected. There is no automatic router conversion.
Plain provider/model identifiers and provider-qualified model pins keep their
normal meanings.

Router authorization uses the requested selector. An access policy granting
`bitrouter/coding` delegates model choice to that router; granting only a
physical model does not authorize the router alias. The pipeline does not
recheck the model allowlist after router selection.

The routing binding digest excludes prompt and parameter values, so it cannot
identify edits confined to those values. It is distinct from the model-policy
source digest and immutable service resource/configuration revisions.

`bitrouter models` adds a router inventory alongside the compatible model catalog.
`bitrouter status` distinguishes `saved_routers`, `running_routers`, and the optional
`router_restart_required` result; unavailable runtime evidence stays unknown.
`bitrouter route bitrouter/coding` reports `router`, `router_source`, `bound_policy`,
and routable `candidate_models`. A dynamic policy preview reports
`policy_decision_executed: false`, labels `effective_model` as the base input,
and leaves `provider_chain` empty: it has not executed the policy or selected
an upstream. Settled request history records optional `router_id`,
`binding_digest`, and `original_selector` beside the actual provider/model.
Old rows keep null identities; these fields are not exported as new trace
attributes in this batch.

### `bitrouter init`

Run `bitrouter init` for interactive gateway setup. For a noninteractive file
use `bitrouter init --yes --use-detected --after exit`. Choose `--after serve`
to start the gateway. Use `--config PATH` to select the destination. Configure
BRO separately with the endpoint of a running gateway and a client credential.

## Routing / introspection

### `bitrouter route <model>`

```
bitrouter route gpt-4o [--prompt <text>] [-c <path>] [--socket <path>]
```

Resolves a model or router selector using the running daemon when reachable, otherwise the local configuration. Fixed routes include the provider fallback chain. A policy-bound router reports its binding and routable candidates with `policy_decision_executed: false`: the preview does not execute its dynamic policy or predict the selected model.

The config fallback probes `auto_discover: true` providers with no declared
models, using the same bounded discovery as `bitrouter models`. A model shown by the
config listing can therefore be previewed by the config route check.

`--prompt` supplies request text for the existing static policy-table preview on the local config path. It does not execute a router's dynamic policy; live previews do not use it.

`bitrouter route --json` returns the shared route action report:

| Field | Meaning |
|---|---|
| `requested_model` | the selector you asked about |
| `effective_model` | resolved model for fixed routes; base model for a dynamic-policy preview, not a selected target |
| `effective_effort` | reasoning effort selected by an executed static preview, when present |
| `resolved_via` | `live` \| `config` \| `zero_config` |
| `router` / `router_source` | router identity, public binding digest, original selector, and configuration source |
| `bound_policy` / `candidate_models` | bound policy and routable candidates; candidates are not a decision |
| `policy_decision_executed` | whether this preview executed a policy decision; false for a dynamic-policy router |
| `policy_decision` | an executed static policy-table preview, when available; absent on live previews |
| `provider_chain[]` | provider / upstream service id / protocol, preferred hop first; empty for dynamic-policy previews |
| `estimated_cost` | first hop's per-token rate card, when a fixed chain is available; not a request total |

Read-only throughout — nothing is sent upstream.

### `bitrouter models`

```
bitrouter models [-c <path>] [-p <provider-id>]
```

Lists all routable models, each with **every** provider that can serve it — the
fallback chain, in order. Filter to one provider with `--provider`.
Subscription providers are explicit-route-only, so their rows use a pinned
`provider:canonical-model` selector (for example,
`openai-codex:openai/gpt-5.6-sol`). Copying any displayed selector into
`bitrouter route` therefore previews the route without implicitly opting a bare
canonical request into a personal subscription.

Queries the running daemon if reachable and falls back to a local config parse,
the same order `bitrouter route` uses: the live routing table reflects `reload`s
and what the daemon actually resolved at start-up, where a static parse is what
the file says now. The parse is resolved the way the daemon resolves its own —
built-in defaults, then providers whose credential lives in the OAuth store
rather than the config (`claude-code`) re-activated — so a
subscription-backed provider is listed with no daemon running. `--json` reports
which view answered as `resolved_via: "live" | "config"`, and the human view
annotates a `config` listing.

The config fallback probes each `auto_discover: true` provider's `/models`
endpoint (bounded: 2s connect, 5s per request; failures leave that provider with
no models rather than failing the command). The daemon path does no such probing.

Uses the same app-owned report as CLI and typed remote control, so
each retained surface preserves one JSON shape.

### `bitrouter providers check <provider>`

```
bitrouter providers check <provider> [--account <label>] [-c <path>]
```

Reads one configured provider and requests its model list. Local HTTP and
online HTTPS use the same check. The command resolves that provider's endpoint
and credential reference, then GETs the catalog for its existing protocol
(Chat Completions, Responses, and Messages use `{api_base}/models`) with the
configured API key, static headers, and upstream timeouts. Providers that use
stored login credentials need a configured API key or static authentication
header for this check. A redirect returns its original HTTP status.

The result is one connection, or one per account when the provider has
accounts. `--account` checks a single label. JSON is the default. Each object
reports `id`, `endpoint`, `protocol`, `answered`, and the model ids seen.
`--human` prints those as ID, PROTOCOL, ANSWERED, and MODELS. `answered` is
true only when the endpoint returned that protocol's list; a list does not
prove generation.
An unresolved `${VAR}` fails that connection and does not dial. The command
does not send a chat, responses, or messages generation, does not change
`active`, and does not write `bitrouter.yaml`. Discovered ids stay in the
command output. `auto_discover` is unchanged: it still runs only at startup
and reload.

The process exits non-zero when any selected connection did not answer. A
remote context cannot run this command; the dial happens on the machine that
reads the local configuration. `active` in `providers list` remains the routing
flag, not the result of this check.

### `bitrouter providers list`

```
bitrouter providers list [-c <path>] [--socket <path>]
```

Local compatibility output prints each provider's id, model count,
routing-active state, and `api_base`. Named remote contexts and the dashboard
use the redacted accepted catalog, which omits API bases and credentials.
`active` means the accepted routing configuration includes the provider, not
that a connectivity probe succeeded.

---

## MCP upstream diagnostics

### `bitrouter mcp check [server]`

```bash
bitrouter mcp check                 # every configured upstream
bitrouter mcp check my-server       # one configured upstream
```

Performs one `tools/list` round trip per selected server and reports transport,
reachability, latency, negotiated tools capability, and advertised tool names.
This is the canonical diagnostic. The `tools` commands below are hidden
compatibility spellings.

### Hidden compatibility: `bitrouter tools`

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

## MCP configuration and gateway inspection

Native skill metadata is independently opt-in through
`skills.mcp_sources.<mcp_servers-key>`, with exact absolute `workspaces` and
either `local: true` or authenticated `api_key_ids`/`user_ids`. Empty source
policy is the default. This policy grants neither workspace nor tool execution
rights. Read-only native Turns may connect to granted skill sources while
exposing no MCP tools. Saved source policy without a configured server grants
no usable access; ordinary MCP removal remains available. Restart applies these
startup-owned changes. Configuration, metadata admission and verified reads do
not prove skill consent, selection or activation. The current wiring and YAML
example are in the [distributed skill reference](../skills/bitrouter/references/cli.md#native-project-instructions-mcp-and-skills-wiring).
`bitrouter status` reports these saved changes under the fixed `skills` restart field;
routing reload rejects additions and revocations without replacing the live policy.

`bitrouter mcp list` lists **configured upstreams**; `show NAME` shows one. Both
read the source without expanding environment references, connecting to MCP,
or starting a daemon. JSON reports include the absolute `source` path and
redacted `servers` with each source map key as `id` and its declared `name`.
Old aliases remain readable; edits require matching key/name and never rename
another entry implicitly. Human output uses `--human`. Header/env values and stdio
argument values are hidden; HTTP userinfo, query and fragment are omitted.

```sh
bitrouter mcp list [--config PATH]
bitrouter mcp show NAME [--config PATH]
bitrouter mcp add NAME --url https://example.com/mcp [--config PATH]
bitrouter mcp add NAME --config PATH --command program -- arg1 'two words'
bitrouter mcp add NAME --from entry.yaml [--config PATH]
bitrouter mcp edit NAME --from entry.yaml [--config PATH]
bitrouter mcp remove NAME [--config PATH]
```

`--from -` reads YAML/JSON from stdin. Input is one complete existing
`McpServerConfig`, without the outer `mcp_servers` mapping:

```yaml
name: example
transport:
  type: http
  url: https://example.com/mcp
  headers:
    Authorization: 'Bearer ${MCP_TOKEN}'
aggregate: false
tool_prefix: example__
```

For stdio, use `transport: {type: stdio, command: program, args: [], env: {}}`.
All existing aggregate/prefix/header/env fields are supported. Map key and
entry name must match. Add rejects an existing name; edit/remove require one.
`edit --from` replaces the **whole entry**: omitted fields take the existing
structure's defaults (aggregate true, no custom prefix, empty args/env/headers).
`--from` conflicts with shortcut transport flags; stdio args require
`--command` and follow `--` after other options.

In a terminal, `add NAME` can ask for transport and URL/program. `edit NAME`
uses `$EDITOR` on one private entry file, then validates the complete candidate
and detects source changes before saving. Outside a terminal, supply complete
flags/input; there is no interactive wait. Editor stdout goes to stderr so the
CLI's stdout stays JSON. Cancellation, editor failure, invalid input and stale
source versions leave the current configuration unchanged.

Config selection follows the existing resolution order (explicit path, cwd,
`$BITROUTER_HOME`, user home). A missing file is created by add at the same
explicit creation path as onboarding; the receipt names its absolute path.
Read/edit/remove do not create an explicitly missing config. Ordinary block
YAML is changed only at the target entry, preserving other source bytes,
comments, order, environment references and line endings. Flow/anchor headers,
complex keys/layouts, mixed line endings, symlink/hard-link targets and other
structures that cannot be edited safely are refused with manual-editor guidance.
A private validation copy may substitute unset variables with placeholders;
saving does not prove those variables or endpoints will work at runtime.

Publication uses the shared per-target configuration lock, an exact source
version check and a same-directory atomic replace. Existing permissions are
retained; new configuration files are private. A change by an external editor
is detected at the check; arbitrary writers must cooperate with the same lock
to serialize changes during publication. A failure after rename while syncing
the parent directory may mean the new file is already saved; its error asks
you to inspect saved state before retrying.

Writes report `activation: saved_only`, `running: unknown` and the restart
requirement for startup-bound MCP resources. They do not launch/install an MCP
server, contact an upstream, reload routes, restart a daemon, or change its
running bindings. Use `bitrouter status` for the selected daemon's own config-state
evidence. Native MCP transport/credential changes require an explicit restart
after active work finishes; ordinary routing reload cannot apply them.

`bitrouter mcp check [server] [--config PATH]` explicitly connects to one configured
upstream, or all, and reports transport, latency, capabilities and tool names.
`bitrouter tools list/status/discover`, aggregation, Skills-over-MCP relay and native
launcher injection retain their existing behavior. OSS does not expose a
first-party origin MCP server.

The hidden legacy `bitrouter mcp search QUERY [--limit N]` still browses the official
registry. Search is separate from configured-server management. Former hidden
registry `list`/stub-only `add` semantics are replaced by the commands above;
search results do not save or install an entry.

---

## Policy, trajectory and Eval commands

### `bitrouter policy`

```text
bitrouter policy init NAME [--router ROUTER] --economy MODEL \
  [--economy-effort LEVEL] [--strong MODEL] [--strong-effort LEVEL] [--config PATH]
bitrouter policy check [--config PATH]
bitrouter policy status [--disk | --view active|disk] [--config PATH] [--socket PATH]
bitrouter policy show [NAME] [--disk | --view active|disk] [--config PATH] [--socket PATH]
bitrouter policy diff ACTIVE CANDIDATE
bitrouter policy publish CANDIDATE --expected-digest SHA256 [--config PATH] [--socket PATH]
bitrouter policy reload [--config PATH] [--socket PATH]
bitrouter policy rollback DIGEST --expected-digest SHA256 [--config PATH] [--socket PATH]
```

Routing Policy uses the public model-only source contract. The conventional file
is `policy.yaml`; an explicit `policy.path` may select another filename containing
the same source format. Explicit missing or invalid files fail without falling
back to the bundled source. A zero-config local installation uses the bundled
`bitrouter/auto` source: three model tiers, strong default, tool-safe fallback,
and no learned routes.

```yaml
policy:
  path: ./policy.yaml
```

```bash
bitrouter policy init coding \
  --strong provider:strong-model \
  --economy provider:economy-model \
  --config bitrouter.yaml
bitrouter policy check --config bitrouter.yaml
bitrouter policy show coding --disk --config bitrouter.yaml
```

Initialization creates the named policy and router, addressed as
`bitrouter/coding`. `--router` defaults to `coding`; pass another id explicitly.
The command does not choose a provider or model. A new router requires both
model arguments; a compatible existing policy router may supply its strong base
model when `--strong` is omitted. Explicit effort is part of the target identity.
`--strong-effort` requires an explicit `--strong`. Initialization, check,
publication and rollback require a file-backed `bitrouter.yaml`; run `bitrouter init`
first if no configuration file exists.
Initialization preserves unrelated chat and harness settings and reports
`config_activation: saved_only`; explicitly reload or start before relying on
saved settings in a running instance.

Each source document has `spec_version: policy/1.0.0-beta.1` and named `policies`.
Targets contain a model and optional effort. Omitted effort preserves caller
effort. Policy selection preserves all admitted context and does not run tools,
activate context treatments or guards, or publish learning results. Classified
routes require the matching `classifier_digest`; absent classifications fall
back to the source's default tier. Router bindings select names literally,
without request variants or fallback to a different policy name.

`status` and `show` default to the selected instance's active state. `--disk`
conflicts with `--view`; use either `--disk` or `--view disk` for saved source.
A remote disk read refers to that host's saved files. `show` checks an optional
name but exports the complete document in `source`, including all named policies.
The surrounding `source_digest` and view/availability metadata are not source
fields. Extract `source` to a separate YAML or JSON file for resubmission.

```bash
bitrouter policy diff policy.yaml candidate.yaml
bitrouter policy publish candidate.yaml --expected-digest <current-source-digest> --config bitrouter.yaml
bitrouter policy rollback <previous-source-digest> --expected-digest <current-source-digest> --config bitrouter.yaml
```

Publication validates the separate candidate against current configuration and
uses the explicit expected digest as compare-and-swap. The source never supplies
its own parent digest or publication authority. Rollback selects a previously
retained current-format source and uses the same explicit CAS discipline. Stale
operations refuse to overwrite newer source. Local writes use file/process
permissions; service revision creation and activation retain their independent
authentication, scopes and CAS checks.

Successful publication and rollback reload a reachable local daemon. An explicit
`--socket` must be reached. If reload is rejected, recovery restores the previous
source with CAS and attempts to reload it; concurrent winners are preserved and
recovery failures are reported. `policy reload` explicitly reloads saved config
and source through the daemon, preserving the last valid runtime snapshot on
validation failure. Offline publication does not establish runtime activation.

Evaluation and snapshot freezing leave active routing unchanged. Clients author
new source from exact-revision evidence, then explicitly publish or create a new
service revision and independently activate it. Experimental BRO algorithms use
separate objects and entry points. ACL creation is independent and uses
[`access-policy create`](#bro-access-policy-create-id).

### `bitrouter trajectory`

Durable trajectory capture and retention are an explicit local opt-in:

```yaml
trajectory:
  enabled: true
  retention_days: 30
  outbox_batch_size: 100
```

`enabled` defaults to `false`; `retention_days` defaults to `30` and must be
positive; `outbox_batch_size` defaults to `100` and must be between 1 and 1000.
Every trajectory setting is restart-only:
changing `enabled`, `retention_days`, or `outbox_batch_size` during reload is
rejected while the last-known-good runtime remains active. Restart the daemon
to apply any trajectory setting change.

Model-only Policy source cannot configure progress guards or context treatment.
Trajectory capture records structural evidence; enabling it does not grant
model Policy any experimental execution or publication authority.

The feature is source- and task-neutral. It stores event structure, bounded
categorical routing facts, exact counters, and keyed/content digests. It does
not store API keys, bearer credentials, prompts, system instructions, tool
arguments, file bodies, or provider-private metadata. Operational Eval records
are redacted digest/count evidence and an `inconclusive` verdict; they are not a
quality score and do not infer task identity or capability from private data.

```text
bitrouter trajectory inspect EPISODE_ID
bitrouter trajectory replay EPISODE_ID
bitrouter trajectory prune --before RFC3339 [--dry-run]

bitrouter trajectory --config PATH inspect EPISODE_ID
bitrouter trajectory --config PATH replay EPISODE_ID
bitrouter trajectory --config PATH prune --before RFC3339 [--dry-run]
```

`--config PATH` is optional and may appear before or after the trajectory leaf
command. When omitted, these commands use the standard config resolution chain.
They always open that selected source's local database and do not accept an
owner argument. A relative SQLite URL is anchored to the selected config home:
the config file's directory, or the implicit BitRouter home (normally
`~/.bitrouter`) for zero-config. It never depends on or changes the caller's
working directory; absolute/memory SQLite and server database URLs retain their
existing meaning.

`inspect` first resolves the globally unique episode, then performs every read
inside its owner scope. It reports correlation source, history completeness,
current structural health, typed route clauses, and event digests. Persisted
trajectory request IDs and Responses native-parent IDs are installation-keyed,
owner-bound opaque identities; external request IDs on the wire, upstream, and
in metering retain their existing semantics. `replay` validates a stable
episode snapshot and compares the newest persisted route checkpoint digest with
a fresh replay. Corrupt histories expose only the first intrinsically invalid
event id/sequence and a stable reason code; a pending route awaiting completion
is not itself corruption. Concurrent appends are retried and are never reported
as corruption. Use global `--json` or `--human` for either view.

`prune --dry-run` returns exact eligible counts without mutation. A real prune
removes delivered outbox rows older than the exclusive cutoff, then terminal
episode history whose last capture is older than the cutoff. Every request in
an episode must be settled or failed, and any associated pending outbox row
preserves the entire episode. Deletes are bounded by `outbox_batch_size`,
owner-scoped, identity-checked, and transactional. The daemon applies the same
rules at startup using `retention_days`; it never fabricates an
`episode_closed` event. If a client later cites a native parent that retention
already removed, BitRouter starts a new `unresolved` / `incomplete` episode
rather than pretending the lost prefix is complete.

For recovery, back up the configured database, run `prune --dry-run`, inspect
important episodes, and run `replay` before destructive pruning. A replay
contention error means the episode kept changing during the bounded read; retry
after traffic quiets. Stable corruption reason codes mean the durable history
needs operator investigation or restoration from backup.

Trajectory metrics are descriptive: request/settlement counts, elapsed time,
categorical route facts and available authoritative token/cost totals. Missing
metering stays absent rather than becoming zero. `history_complete=false` means
the visible prefix is not proven complete; it does not authorize reconstructing
missing evidence or changing model-policy execution.

### `bitrouter eval`

```text
bitrouter eval subject seal DRAFT --output SEALED
bitrouter eval subject put FILE [--config PATH]
bitrouter eval subject get EVAL_ID [--config PATH]
bitrouter eval subject list [--config PATH]
bitrouter eval result submit FILE [--config PATH]
bitrouter eval snapshot freeze [--at RFC3339] [--config PATH]
bitrouter eval snapshot get SHA256 [--config PATH]
bitrouter eval status [--config PATH]
```

Eval subjects use schema 2 with a closed identity profile. Local gateway
exchange uses `gateway_generic`; tenant exchange distinguishes `tenant_generic`
from `tenant_model_policy`. Model-policy evidence binds installation, tenant,
configuration revision, full immutable Policy `ResourceRef`, and literal selected
name. Source/classifier digests are additional evidence, not substitutes for
resource identity. Results inherit their validated subject identity and retain
result schema 1. Use the tenant HTTP/SDK exchange and exact revision-bound
inference for tenant evaluation; a local gateway ledger supplies no tenant
authority. Generic evidence cannot qualify another Policy revision for activation.

`subject seal` sorts redacted evidence, computes its canonical digest, validates
the completed subject and writes deterministic JSON without opening a ledger.
It does not evaluate the subject or create an active Policy revision.

Generic eval sits outside the inference hot path. Clients submit redacted
subjects and externally produced results. A task-native runner, human,
enterprise system, or agentic evaluator submits the same immutable
`EvaluationResult` via CLI or authenticated REST. BitRouter validates evaluator authority and metric
scope, retains rejected/disputed outcomes, and admits trusted results into a
content-addressed snapshot. It does not bundle or execute a universal judge.
The local CLI operates in the `local` ownership scope. Authenticated REST
operations are isolated to the virtual key's owning user, including list/get,
result submission, status, and snapshot access.

A snapshot commits the exact subject and result digests, is bound to its owner
scope, and excludes held-out, rejected, and disputed results. For episodes with
multiple route decisions, `decision_credit.metric_ids` determines which
decision receives verdict, cost, latency, or violation evidence; omitted credit
is implicit only for a single-decision subject.

The daemon exposes the same library operations at
`GET/POST /v1/evals/subjects`, `GET /v1/evals/subjects/{eval_id}`,
`POST /v1/evals/results`, `POST /v1/evals/snapshots`,
`GET /v1/evals/snapshots/{evidence_root}`, and `GET /v1/evals/status`.
These endpoints mutate evidence only; evaluation does not edit or activate a Policy source.

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

### `bitrouter providers login <provider>`

```
bitrouter providers login claude-code     # Claude Pro/Max subscription via Claude Code
bitrouter providers login openai-codex    # ChatGPT subscription via Codex
bitrouter providers login github-copilot  # GitHub device-code flow
bitrouter providers login openai --api-key sk-…        # BYOK, non-interactive
printf %s "$KEY" | bitrouter providers login anthropic --key-stdin
```

Runs the provider's OAuth flow (PKCE in a browser or device-code, depending on provider) and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The slot is keyed by `(provider_id, label)` — pass `--label <name>` (defaults to `default`) to keep multiple accounts of the same provider side by side. Other providers fall back to a pasted API key.

For a provider that accepts a pasted key, `--api-key <KEY>` (or `--key-stdin`, which reads one line from stdin) seeds it non-interactively — skipping the method menu and the paste prompt. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For the built-in `bitrouter` provider the key seeds the cloud credential, exactly as `cloud login --api-key` does.

For `claude-code`, the login menu defaults to the live Claude Code session. For `openai-codex`, the default is **"Import an existing session from the vendor CLI"** — BitRouter reads the credential Codex already stored in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS Keychain, and adopts it with no fresh browser sign-in. The imported token refreshes automatically like any other; choose the browser subscription flow when no local Codex session exists.

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bitrouter cloud login`](#bro-cloud-login--logout--whoami) below.

### `bitrouter providers logout <provider>`

```
bitrouter providers logout github-copilot
```

Removes every stored credential for the provider (subscription OAuth tokens and pasted API keys alike).

### `bitrouter cloud login` / `logout` / `whoami`

Cloud sign-in, distinct from the per-provider `bitrouter providers login` flow above. Interactive login uses the RFC 8628 OAuth Device Authorization Grant. For CI and other non-interactive environments, pass an existing BitRouter API key with `--api-key`. Both forms persist to the same credential file and are reused by `cloud api`, management commands, the built-in `bitrouter` provider, and account-attributed telemetry.

OAuth browser approval asks which workspace to bind; the resulting credential is **namespace-baked** (workspace-baked). To switch workspaces, re-run `bitrouter cloud login`. OAuth credentials auto-refresh on use. API-key login performs no network request and management commands use the server's `me` namespace alias.

```
bitrouter cloud login [--oauth-as <URL>] [--client-id <ID>] [--scope <SCOPE>]
bitrouter cloud login --api-key <BRK_API_KEY> [--oauth-as <URL>]
bitrouter cloud logout [--oauth-as <URL>] [--client-id <ID>]
bitrouter cloud whoami
```

| Flag | Default | Description |
| --- | --- | --- |
| `--oauth-as` | `https://api.bitrouter.ai` (env: `BITROUTER_OAUTH_AS`) | Authorization server base URL — override only for a self-hosted deployment |
| `--client-id` | `bitrouter-cli` (env: `BITROUTER_OAUTH_CLIENT_ID`) | Public OAuth client id |
| `--scope` | broad developer set (env: `BITROUTER_OAUTH_SCOPE`) | Space-delimited scopes to request. Default includes `inference:invoke`, `usage:read`, `keys:read`/`write`, `billing:read`, `policy:read`/`write`, `byok:read`/`write`, `namespace:read`. Sensitive control-plane scopes such as `billing:write`, `user:write`, and `namespace:write` are opt-in. |
| `--api-key` | *(none)* | Store a `brk_<token_id>.<secret>` credential without browser login or network discovery. Conflicts with `--client-id` and `--scope`; intended for CI. |

Credentials are persisted at `<data-dir>/account-credentials.json` (mode `0600` on Unix). Existing untagged OAuth files remain compatible. `whoami` answers from the local file with no network call and reports `authentication: oauth|api_key` without printing a bearer. OAuth logout attempts RFC 7009 revocation before deleting the file; API-key logout is local-only.

---



## Access policies

### `bitrouter access-policy create <id>`

```
bitrouter access-policy create strict [--dir ./policies]
```

Writes a starter access-control file under `./policies` by default, independently
of routing `policy.yaml`. Existing ACL files are not overwritten. Bind it to a key with:

```
bitrouter key sign --user <id> --policy strict
```

---

## Cloud account management

`bitrouter cloud …` drives the BitRouter Cloud API using the credential persisted by [`bitrouter cloud login`](#bro-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command. Typed subcommands cover the common terminal workflows: namespace inspection, API keys, usage and request history, billing balance and checkout, policies, budgets, presets, and BYOK. Use `bitrouter cloud api <relative-endpoint>` for the rest of the Cloud API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, and namespace/account lifecycle endpoints.

OAuth credentials are **namespace-baked** — keys, usage, and policies are scoped to the workspace chosen at login. API-key credentials use `/v1/namespaces/me/*`. The path segment is always resolved implicitly; callers never pass a workspace argument. `billing` and `byok` are user-level and reach across all workspaces regardless.

Every leaf accepts `--json` to print the raw response body instead of the human-readable summary. On a 403 whose description is `missing required scope: <s>`, OAuth users receive a copy-pasteable re-login hint that appends the missing scope; API-key users are told to mint or select a key with that scope and log in with it.

### `bitrouter cloud api`

Make an authenticated request to any **relative** endpoint on the origin recorded by `cloud login`, modeled after [`gh api`](https://cli.github.com/manual/gh_api):

```bash
bitrouter cloud api /v1/models
bitrouter cloud api /v1/chat/completions --input request.json
bitrouter cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

```text
bitrouter cloud api <ENDPOINT> [-X <METHOD>] [-H <KEY:VALUE>] \
  [-f <KEY=VALUE>] [-F <KEY=VALUE>] [--input <FILE|->] \
  [-i|--include] [--silent|--verbose]
```

| Flag | Behavior |
| --- | --- |
| `-X`, `--method` | Explicit HTTP method. Default is `GET`, or `POST` when fields or `--input` are present. |
| `-H`, `--header` | Append a request header; repeat to send multiple values. A supplied `Authorization` overrides the stored bearer. |
| `-f`, `--raw-field` | Add a string field. Supports `key[subkey]` and `key[]` nesting; `key[]` without `=` creates an empty array. |
| `-F`, `--field` | Add a typed field. `true`, `false`, `null`, and integers become JSON types; `@file` and `@-` read a string value from a file or stdin. |
| `--input` | Use exact file bytes (or stdin with `-`) as the request body. Fields become query parameters. |
| `-i`, `--include` | Prepend the HTTP status line and response headers to stdout. |
| `--silent` | Drain but do not print the response body. Conflicts with `--verbose`. |
| `--verbose` | Print method, URL, status, and headers to stderr. Credential-like header values are redacted. |

With explicit `GET`, fields are query parameters. Otherwise fields form a JSON body unless `--input` owns the body. Only one consumer may read stdin. Non-TTY response bytes and SSE are streamed unchanged; interactive JSON is pretty-printed. On HTTP 4xx/5xx, the response body remains on stdout, the diagnostic goes to stderr, and the process exits non-zero.

Absolute URLs, scheme-relative paths, fragments, and cross-origin redirects are rejected. Redirect following is disabled, so a stored bearer is never forwarded to another origin. Documented endpoints include `/v1/models`, `/v1/providers`, `/v1/stats/usage`, `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, namespace-scoped management routes under `/v1/namespaces/*`, and user-level routes under `/v1/account`, `/v1/billing/*`, and `/v1/byok/*`.

This first release intentionally omits `gh api`'s GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname, preview, and placeholder expansion features.

### `bitrouter cloud whoami`

```
bitrouter cloud whoami
```

Prints the cloud identity and the bound namespace alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bitrouter cloud namespace`

Inspect the workspaces you own and the one this CLI session is baked to. The typed CLI only inspects workspaces; creation and deletion require the Console or `bitrouter cloud api` with the appropriate control-plane scope.

```
bitrouter cloud namespace list    [--json]
bitrouter cloud namespace current [--json]
```

`list` fetches all namespaces you own and marks the active one. `current` is offline — it reads the local credential and prints the bound namespace id without a network call. If the credential predates namespace-scoping, it prints `(no namespace — run \`bitrouter cloud login\`)`.

### `bitrouter cloud keys`

Manage `brk_` API keys in the active workspace. All minted keys are workspace-baked to the same namespace as the caller and cannot upscale their scopes beyond the caller's.

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

User-level — not workspace-scoped; reflects the account-wide wallet regardless of which workspace the CLI is signed in to.

```
bitrouter cloud billing balance [--json]
bitrouter cloud billing checkout --amount-cents <N> [--json]
```

`checkout` starts a Stripe credit-purchase session and prints the hosted URL. Requires the `billing:write` scope, which is opt-in — pass `--scope` to `bitrouter cloud login` to request it.

Use `bitrouter cloud api /v1/billing/transactions` for the billing ledger, and `/v1/billing/checkout/sessions/<session-id>/status` for checkout status.

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

`--spec` reads the flat inner spec body as JSON from a file path or `-` for stdin. Principal types: `namespace`, `api_key`, `oauth_token`, `oauth_client`. `disable` parks a policy without deleting it — the engine skips disabled rows at request time.

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

User-level — not workspace-scoped; BYOK provider keys are account-wide. The cloud only stores already-sealed ciphertext — seal against the cloud's current X25519 public key (separate fetch) before calling.

```
bitrouter cloud byok list [--json]
bitrouter cloud byok set    --provider <ID> --ciphertext-b64 <B64> --kek-id <ID> --key-prefix <PREFIX> [--api-base <URL>] [--json]
bitrouter cloud byok delete <PROVIDER> [--json]
```



## Request-check extensions

There is no `bro checks` command or request-check receipt API. Validate native
declarations with `bitrouter config validate`; custom-host activation additionally
verifies compiled registrations and revisions. Checker declarations and router
bindings require restart after changes. Startup logs and bounded invocation
tracing provide content-free diagnostics; absence of telemetry is not proof that
a check did not run. Existing `bitrouter requests` remains the settled cost/usage
interface. See [Router request checks](development/extensions.md#request-entry-and-checking) for the execution
and coverage contract.

## Split a stopped M0 SQLite store

Stop the M0 owner and preserve its external keys and configuration. Choose a
fresh destination directory on the same filesystem as the work area.

```sh
bitrouter tenancy offline split-store --database /path/to/stopped.sqlite --destination /path/to/inspection
bitrouter tenancy offline split-store --database /path/to/stopped.sqlite --destination /path/to/active --activate
```

The first command preserves a rollback snapshot and creates inspection copies.
Activation verifies both projections and reports blockers before exposing a
runnable pair. Activation retires the original M0 database path. Repeat an
interrupted publication with the same destination. The byte-exact rollback
image is `rollback/source.sqlite`. Use the returned gateway database with its original service keys
and the returned BRO directory as `BRO_HOME`. Check pending usage and tool effects
at both owners before reopening admission or reverting an active installation.

## Offline DB evidence (Unix SQLite only)

`bitrouter tenancy offline` operates on explicitly supplied local paths. It does not
load host configuration, discover a daemon, contact HTTP, migrate a source, or
bootstrap an installation. Stop all source writers before capture. The existing
canonical exclusive DB ownership lock refuses participating live writers,
including symlink aliases; hard-linked DB files are rejected. Old/external
writers that do not participate in this lock still require an external fence.

```sh
bitrouter tenancy offline snapshot --database /absolute/state/bitrouter.db --bundle /absolute/new-db-evidence
bitrouter tenancy offline inventory --bundle /absolute/new-db-evidence
bitrouter tenancy offline verify --bundle /absolute/new-db-evidence --evidence-digest <digest-from-snapshot>
```

Snapshot defaults to the bounded `v1` evidence artifact. Use `--format v2`
with an existing private scratch directory for complete processing with finite
working memory:

```sh
bitrouter tenancy offline snapshot --database /absolute/state/bitrouter.db \
  --bundle /absolute/new-db-evidence --format v2 \
  --scratch-directory /absolute/private-scratch
bitrouter tenancy offline restore --bundle /absolute/new-db-evidence \
  --evidence-digest <digest-from-snapshot> --database /absolute/new-state/bitrouter.db \
  --expected-installation-id <original-installation> --wrapping-key-env WRAPPING_KEY \
  --scratch-directory /absolute/private-scratch
```

Inventory, verify, audit and recover also accept explicit scratch and working
budgets. `--working-bytes` defaults to 67108864, `--scratch-free-bytes` to
268435456, and `--validation-page-rows` to 32. `--validation-bytes` is the alias
for the working allowance. These bound temporary work, not retained history.
Restore requires scratch, the original installation and wrapping key, and a new
literal database path in a private directory. It preserves current dataset
identity, ciphertext and history; it does not migrate, merge, reset counters,
start the service or release ingress. Do not run original and restored
installations concurrently. Artifact v1/v2 selection does not make historical
authority or Policy formats readable. Historical capacity upgrades and old Eval
backfills have no command entry point.

The bundle path must not exist. Preserve the returned `evidence_digest` outside
the bundle and pass its exact64 lowercase hexadecimal characters to `verify`.
Inventory recomputes the actual contents but does not authenticate the origin of
a self-contained bundle. Verification compares the supplied evidence anchor,
private profile, file digest and actual typed SQL inventory. Neither operation
grants execution, migration, owner assignment or recovery authority.

SQL-value inventory verification does not authenticate every encrypted service
payload or establish runtime recovery eligibility. Authenticated service
validation and current-store/authority checks remain separate gates for audit,
recovery and restore.

The snapshot, inventory and verify results have `scope: sqlite_database_only`, `complete: false` and
`cutover_ready: false`. This is **not a full backup** or an M0 MigrationManifest.
External OAuth files, workspace/harness state and external-effect outcomes are
unassessed, excluded and unfenced. Source build/commit are honestly null; the
report separately identifies the inspecting binary. Missing installation or
ledger is recorded as null, never created or represented as an empty ledger.
No configuration input is accepted; `--config`, remote endpoint/token flags and
`--context` are rejected. If a deployment selects external state, this profile
cannot qualify that deployment for cutover; preserve it separately under a
qualified operator procedure. Never use a newly introduced sidecar as evidence
that an old OAuth writer has stopped.

The new directory is0700; `snapshot.sqlite`, `inventory.key`, `evidence.json`
and `READY` are0600. The SQLite copy and inventory key are sensitive; reports
contain metadata, counts and keyed digests, not raw records/SQL definitions or
secrets. Keep the whole secured bundle together. A failed or interrupted command before publication
leaves `INCOMPLETE`; inventory/verify refuse it. Do not remove that marker or
resume the output. Inspect/quarantine it and select a new directory for retry.

The private v1 profile bounds logical/file DB size to512MiB,256 tables,2048 schema
objects,128 columns/table,1MiB/schema SQL,16MiB/cell,32MiB/row,256MiB inspected
values/schema bytes,1,000,000 rows and2MiB evidence JSON. Exceeding a bound fails
without truncating counts. Virtual/shadow tables and generated/hidden columns
are unsupported. Ordinary views/triggers are hashed as schema, never executed.
Inventory frames SQLite null/integer/real/text/blob distinctly, preserves
duplicates, and hashes the sorted multiset of keyed row digests. Text/blob bytes
are not parsed as JSON. This is typed SQL-value preservation, not physical-page
or hidden-rowid preservation; SQLite VACUUM may change physical layout.

Transfer, new credentials, OAuth transfer, operation-capacity recovery and
034/035/036-specific recovery are outside snapshot, inventory and verify. A
successful exit only means the requested **partial DB evidence** operation completed.

The offline inspector qualifies source definitions before integrity checking or
copying. Views/triggers, expression/partial indexes and executable schema
expressions outside its bounded literal/comparison profile are refused. Existing
BitRouter continuation CHECK constraints remain supported. Per-connection VM
work is bounded; a cancelled operation retains its source owner through actual
driver cleanup. Unconfirmed connection initialization or close retains the
canonical source fence until process exit; it is not a successful snapshot.

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

Counts and evidence inside the manifest remain operator claims. The command does **not** authenticate external ownership/producer evidence, count per-owner source records, backfill rows, provision a target or retire writers. Missing installation identity remains explicitly unobserved. All completion/cutover flags stay false. This read-only packet does not enable historical data import or conversion. A new installation is provisioned through current APIs; backup restore requires a supported current dataset.

### Fresh installation recovery verification (Unix SQLite)

`bitrouter tenancy offline recover --plan FILE --report NEW_DIRECTORY` verifies an
anchored retired archive against an already stopped, freshly provisioned target.
The private plan names the actual new installation, complete provisioning results,
approved static definitions, selected external files, and anchored operator
retirement evidence. All paths are explicit; no ambient configuration or daemon
discovery supplies inputs. The report directory must be new.

Use normal Service/API/SDK operations to provision the target and fresh ingress
credentials first. This command reads the existing records and authenticates the
static resource/configuration closure with the explicitly named wrapping-key
environment variable. It does not provision, migrate, transfer keys/history,
retry uncertain effects, or release ingress. The original source namespace and
operation counters remain in the archive. See [the recovery profile](RESOURCE_TENANCY_RECOVERY.md)
for private plan fields, file and work limits, and the supported static closure.

The secured report retains `complete=false`, `cutover_ready=false`,
`external_writers_independently_excluded=false`, `history_transferred=false`,
and `upstream_effects_resolved=false`. Matching retirement evidence is an operator
attestation, not proof of process exit. Keep source retirement and target ingress
fences in place. `VERIFIED` authenticates report bytes only and requires an absent
`INCOMPLETE` marker; it does not authorize runtime work.
