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
without that feature fails at startup or reload. Saved loop
tuning with no selected tool remains readable and does not activate execution.

Model-only Policy and the tenant Eval exchange are available in the minimal
gateway. Evaluation runs in a client or external evaluator. Submitting or
freezing evidence preserves the active Policy until an authorized activation.

## Executable identity

`bitrouter build-info` reports the service version, source revision, target,
effective features and distribution profile. `bro build-info`
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

> Non-reporting commands are exempt: `serve` is a long-running server; `acp serve` is a stdio JSON-RPC bridge; `bro task run` and `bro acp prompt` emit NDJSON, and `bitrouter cloud api` streams the remote response body.

## Local BRO threads and agents

BRO connects to the gateway origin selected by `--endpoint` or
`BITROUTER_ENDPOINT` and authenticates with `BITROUTER_API_KEY`. The default
origin is `http://127.0.0.1:4356`. Local checkpoints live in `BRO_HOME`
(default `~/.bro`) or the explicit `--data-dir`. Each thread records the
workspace and gateway at creation.

```sh
bro task run 'Summarize this project' --workspace . --read-only
bro task run 'Update the example' --workspace . --check './verify.sh'
bro thread list
bro thread history THREAD_ID
bro thread pending THREAD_ID
bro thread answer THREAD_ID --turn TURN_ID --request REQUEST_ID allow
bro thread resume THREAD_ID --turn TURN_ID
bro thread cancel THREAD_ID --turn TURN_ID
```

`--check COMMAND` authorizes one sandboxed invocation after the model finishes.
The terminal event includes the verification verdict, exit status, bounded stdout
and stderr, interpreter, and timeout evidence. An interrupted check with unknown
effects requires recovery. Resume reuses a saved result. Starting a later turn
on the same thread requests fresh approval for its check. Verification requires
a coding workspace and a supported shell sandbox.

On macOS, the native shell and `--check` run under Seatbelt in a private working
copy. Use workspace-relative paths. BRO copies single-link regular files and
directories on the workspace volume. Symlinks, hard links, special files and
set-ID files are excluded. The copy supports 16 MiB per file, 64 MiB of file
contents, 16,384 entries and 64 directory levels. An oversized input refuses
the command before launch.

Workspace changes become visible after the command stops. BRO rechecks the
input inventory and file contents before accepting the result. It publishes file
contents, ordinary permission bits and regular-file modification times through
the original directory handle. Existing files retain their ownership and extended
attributes when edited. BRO refuses removal or replacement of entries with
extended ACLs or custom extended attributes, including resource forks.
On case-insensitive volumes, case-only renames keep the existing file or
directory identity. BRO stages type replacements before moving the original
entry aside. A publication conflict records the command's exit status and
output and requires recovery. Earlier writes may already be durable. If
publication started, the error gives the paths to the desired working copy and
the admitted original files. Inspect both before removing them. Resuming the
turn shows that evidence without running the shell again.

Use the IDs emitted by BRO. Native task output is NDJSON. An exact approval
authorizes a single recorded call. When an operation may have reached a model
or changed the workspace before its result was committed, recovery reports an
unresolved operation. A completed turn can be inspected after a process restart.

Use `bro acp list` to see bundled adapters and their headless gateway routing
support. For example, `bro acp --workspace . prompt --agent codex 'Inspect this
project'` runs an installed adapter with the selected gateway credentials.

### Local skills

Run `bro skills list` to inspect project skills, or add `--global` or `--root`
to select another source. `show`, `read`, `write` and `init` operate on source
files. Select project skills on `bro thread create` or `bro task run` with
repeated `--skill NAME` arguments. BRO freezes the selected instructions at
thread creation and retains their digests for `bro skills explain --thread ID
--turn ID`. Reads and approvals use the thread's existing permissions.

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
- **Turn on `bitrouter::observe::span_attributes` when a forwarded attribute does not appear on a span.** A deployment can attach its own attributes to the root `chat` span, but the span schema reserves its own vocabulary: keys under `bitrouter.` or `gen_ai.`, and any key the schema already declares (`$screen_name`, `error.type`, `server.address`, …), are **dropped rather than stamped**, so one deployment cannot redefine what an attribute means for everyone else. The drop is deliberate and per-request, hence DEBUG rather than WARN: `RUST_LOG=info,bitrouter::observe::span_attributes=debug` names each dropped key. The full reserved region is `apps/bitrouter/span-schema.json`.
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
- The gateway logs a warning for each unread plugin ID during startup. BRO
  reads its own workspace and adapter settings independently.

The supported ids the binary reads are `bitrouter-policy` and
`bitrouter-telemetry`. The removed `plugins.bitrouter-guardrails` key instead
fails validation and activation, including empty/null values; see
[the custom-host setup guide](GUARDRAILS_EXTENSION.md). A dead sub-key under a live id is reported too, so a
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
host reload operations use separate IDs. Inspect the tenancy capabilities before
requesting revision-bound dispatch.

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
and its exact selected name. The importer accepts static API-key credentials
with generation zero. The manifest is
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

Closes admission, drains in-flight requests including streaming responses, and
then starts the replacement daemon after the current process has stopped. Use
the owning service manager to restart a foreground deployment.

### Local daemon after an update

`bitrouter update` installs the replacement executable and prompts you to
run `bitrouter restart`. `bitrouter update --check` only reports availability.
Check `bitrouter status` after restarting to confirm the running version.
`bro update` uses its own install receipt. Start a new BRO invocation to use
the updated harness. See [distribution and updates](DISTRIBUTION.md#updates-and-profile-transitions).

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

CLI and typed remote control use the same application report.

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

The figure covers all callers of this daemon, the same scope `requests` reports.

`bitrouter requests` reports what the router has actually done: newest-first
settled requests — time, model, the provider that **actually** served, tokens
in/out, cost, latency, status — plus daemon state and the window's spend and
trailing-minute rate. It reads the metering store directly, so it also works
with **no daemon running**. `--since` and `--until` are an RFC3339 pair with a
maximum seven-day interval; absent both, the server selects today from UTC
midnight. `--model` and `--provider` filter the same report before the limit.
`status --requests` remains a hidden compatibility spelling.

Like every other report it uses JSON by default and `--human` for the table. Repeat it with `watch -n1 bitrouter requests --human` for a live view.

**Spend is reported only where there is evidence.** An unpriced request shows `?`.

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
bitrouter route gpt-4o [-c <path>] [--socket <path>]
```

Resolves a model or router selector using the running daemon when reachable, otherwise the local configuration. Fixed routes include the provider fallback chain. A policy-bound router reports its binding and routable candidates with `policy_decision_executed: false`: the preview does not execute its dynamic policy or predict the selected model.

The config fallback probes `auto_discover: true` providers with no declared
models, using the same bounded discovery as `bitrouter models`. A model shown by the
config listing can therefore be previewed by the config route check.

`bitrouter route --json` returns the shared route action report:

| Field | Meaning |
|---|---|
| `requested_model` | the selector you asked about |
| `effective_model` | resolved model for fixed routes; base model for a dynamic-policy preview, not a selected target |
| `resolved_via` | `live` \| `config` \| `zero_config` |
| `router` / `router_source` | router identity, public binding digest, original selector, and configuration source |
| `bound_policy` / `candidate_models` | bound policy and routable candidates; candidates are not a decision |
| `policy_decision_executed` | whether this preview executed a policy decision; false for a dynamic-policy router |
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

The service owns its configured MCP upstreams and the metadata it exposes to
authorized callers. BRO owns workspace skill selection and pins the selected
content in the local thread checkpoint. See [local skills](#local-skills).

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

Routing Policy uses the public model-only source contract. Configure a named
router and its source before requesting it. The conventional file is
`policy.yaml`, and `policy.path` can select another path. If a configured
source is missing or invalid, validation fails.

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
Initialization reports `config_activation: saved_only`; explicitly reload or start before relying on
saved settings in a running instance.

Each source document has `spec_version: policy/1.0.0-beta.1` and named `policies`.
Targets contain a model and optional effort. Omitted effort preserves caller
effort. Policy selection preserves all admitted context. Classified routes use a
typed `RouteClass` and the matching `classifier_digest`. Without a classification,
the selected policy uses its default tier. Router bindings select a literal
policy name.

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
service revision and independently activate it. ACL creation uses
[`access-policy create`](#bitrouter-access-policy-create-id).

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

Trajectory capture records structural evidence and leaves routing decisions to
the selected model Policy.

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
current structural health and event digests. Persisted
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

Eval subjects, results and frozen snapshots carry `spec_version: eval/1.0.0-beta.1`.
Local gateway exchange uses `gateway_generic`; tenant exchange distinguishes `tenant_generic`
from `tenant_model_policy`. Model-policy evidence binds installation, tenant,
configuration revision, full immutable Policy `ResourceRef`, and literal selected
name. Source/classifier digests are additional evidence, not substitutes for
resource identity. Results inherit their validated subject identity. Use the
tenant HTTP/SDK exchange and exact revision-bound
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

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bitrouter cloud login`](#bitrouter-cloud-login--logout--whoami) below.

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

`bitrouter cloud …` drives the BitRouter Cloud API using the credential persisted by [`bitrouter cloud login`](#bitrouter-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command. Typed subcommands cover the common terminal workflows: namespace inspection, API keys, usage and request history, billing balance and checkout, policies, budgets, presets, and BYOK. Use `bitrouter cloud api <relative-endpoint>` for the rest of the Cloud API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, and namespace/account lifecycle endpoints.

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

## Offline DB evidence (Unix SQLite only)

`bitrouter tenancy offline` takes explicit local paths and works on a stopped,
current-format SQLite gateway database. Stop all writers first. The exclusive
database lock fences participating writers. Preserve external credentials and
other deployment state separately.

```sh
bitrouter tenancy offline snapshot --database /absolute/state/bitrouter.db \
  --bundle /absolute/new-db-evidence \
  --scratch-directory /absolute/private-scratch
bitrouter tenancy offline verify --bundle /absolute/new-db-evidence \
  --evidence-digest "$EVIDENCE_DIGEST" \
  --scratch-directory /absolute/private-scratch
bitrouter tenancy offline restore --bundle /absolute/new-db-evidence \
  --evidence-digest "$EVIDENCE_DIGEST" --database /absolute/new-state/bitrouter.db \
  --expected-installation-id "$INSTALLATION_ID" --wrapping-key-env WRAPPING_KEY \
  --scratch-directory /absolute/private-scratch
```

Choose a new bundle directory and a private scratch directory. Keep the bundle
private and retain its evidence digest separately as `EVIDENCE_DIGEST`. An
interrupted publication can leave the bundle marked `INCOMPLETE`. Verify and restore require a completed
bundle, and restore also requires the installation identity and original
wrapping key. The restored database preserves its identity, receipts and
authority state. Use a new destination and keep the source stopped while
bringing the restored database into service.
