# Spec: remote router administration

Status: **implemented; expanded live verification found a remote MCP issue.**
The original acceptance run completed on 2026-09-08 in the maintainer-approved
Docker environment. A subsequent live MCP test found that the transport rejects
the external Host header through a TLS proxy that preserves it. See the
[acceptance evidence and current findings](REMOTE_ADMINISTRATION_PROGRESS.md).
Date: 2026-09-08.
Verified source baseline: `b657cb62` (`v1.0.0-alpha.30`).

## 1. Decision to review

Expand the existing named-context HTTP client into a router administration
surface. The first release adds operational reads and one write: reloading the
server's existing configuration. The CLI and Code dashboard use the same typed
actions, target resolution, authorization, and reports.

An operator can inspect the running router, distinguish its active policy from
files waiting on disk, diagnose telemetry, inspect its configured agent catalog,
and reload changes prepared on the server. They can determine whether a reload
succeeded, failed before applying changes, or partially applied changes even if
the initiating HTTP connection was lost.

This release continues the trusted-operator deployment model. It introduces
separate read and reload authority, not tenant isolation or remote code
execution. Provider credentials and configuration files stay on the host.

### Review decisions

| ID | Recommended decision | Consequence / alternative |
| --- | --- | --- |
| D1 | Ship inspection plus guarded reload as the first administration release. | Policy publication, remote config editing, and service supervision need later contracts. Reads can land first, but do not constitute the full release. |
| D2 | Preserve the existing token as a read-only operator credential; add explicit named credentials with scopes. | Existing deployments gain no write authority merely by upgrading. A single all-powerful token would be smaller but would silently expand existing authority. |
| D3 | Report reload's actual per-subsystem outcome; do not promise transactional rollback. | Strengthen preparation before mutation, but a partial commit remains an explicit terminal result. An all-or-nothing runtime swap is a separate, larger refactor. |
| D4 | Keep remote exposure in one app-owned inventory joined to existing shared action IDs. | Reuse shared types; avoid forcing administration operations into MCP's reversible session-action model. |
| D5 | Add the new reads to CLI and dashboard; preserve the current three-tool daemon MCP profile for this release. | Share its authorization and redaction machinery now. Wider MCP tool exposure remains an explicit later decision. |
| D6 | Exclude active agent/MCP diagnostics. | `agents check` launches an agent; MCP checks can launch configured subprocesses. These are not passive reads. |

The maintainer authorized implementation of this spec, including D1–D6, on
2026-09-08. Flags, paths, scopes, and configuration below remain implementation
requirements, not proof of delivered behavior. Track evidence in the
[implementation ledger](REMOTE_ADMINISTRATION_PROGRESS.md).

## 2. Verified starting point

| Source | Current behavior and implication |
| --- | --- |
| [`remote_control.rs`](../apps/bitrouter/src/remote_control.rs) | `/control/v1` exposes capabilities, status, models, route preview, and requests. The dedicated listener is disabled by default, loopback-only, and validates `BITROUTER_CONTROL_TOKEN`. |
| [`contexts.rs`](../apps/bitrouter/src/contexts.rs), [`main.rs`](../apps/bitrouter/src/main.rs) | Contexts store endpoint and token environment-variable references. Remote command eligibility is a manual match; supported commands reject local config/socket overrides. |
| [`dashboard.rs`](../apps/bitrouter/src/dashboard.rs) | Local/remote dispatch is repeated in the app driver. Snapshot refresh uses `try_join!` across status/models/requests, so one error prevents the full snapshot from updating. Remote agent lists are empty. |
| [`actions/mod.rs`](../crates/bitrouter-mcp/src/actions/mod.rs) | Shared CLI/MCP/TUI actions already have IDs, schemas, reach, and effects. `Effect::Write` requires a real inverse; reload does not fit that promise. |
| [`actions/requests.rs`](../apps/bitrouter/src/actions/requests.rs) | Requests are host-wide, limited to 1–500 rows and today's window. Metering failures become empty/default data, so absence of data and failure are not fully distinguishable. |
| [`server.rs`](../crates/bitrouter-mcp/src/server.rs) | `local_http_router` mounts the caller-assembled MCP profile at `/mcp-control`. The daemon wires status, models, and route preview into it under control authentication. This is distinct from the crate's restricted cloud HTTP profile. |
| [`actions/status.rs`](../apps/bitrouter/src/actions/status.rs) | Status includes host-wide spend and the local socket path. REST removes the path; the daemon MCP profile receives the unfiltered status port. Redaction currently depends on transport wiring. |
| [`main.rs`](../apps/bitrouter/src/main.rs), [`policy_lock.rs`](../apps/bitrouter/src/policy_lock.rs) | CLI policy status/show load files. The running policy runtime has its own snapshot/digest. A file report must not be presented as proof of the live policy. |
| [`reload.rs`](../apps/bitrouter/src/reload.rs) | Reload has a mutex and prepares policy-runtime state, but swaps policy-table, routing, timeout, and policy-runtime state sequentially, then reloads access policies. Errors can follow successful changes. |
| [`daemon.rs`](../apps/bitrouter/src/daemon.rs) | Local reload accepts environment overrides and installs them before calling the reloader. This owner-trusted command is not a public remote request type. |
| [`agents.rs`](../apps/bitrouter/src/agents.rs) | Agent listing is passive, but custom descriptions contain command arguments. Agent checking spawns processes and initializes ACP. |

The [remote-control MVP](REMOTE_CONTROL_MVP_SPEC.md) remains the historical
baseline. This proposal supersedes its read-only limit only for the explicitly
authorized reload action. The [remote ACP RFD](REMOTE_CLI_TUI_SUPPORT_SPEC.md)
remains deferred; its administration exclusions and older command names must
not be mistaken for this proposal's action matrix.

## 3. Release scope and action matrix

All subjects below are the selected server host. No request selects an arbitrary
filesystem path, workspace, shell command, environment value, or API principal.

| Canonical action | CLI with `--context <name>` | HTTP under `/control/v1` | Scope | Dashboard |
| --- | --- | --- | --- | --- |
| `status` | `status` | `GET /status` | `control:read` | Overview |
| `list_models` | `models [--provider ...]` | `GET /models` | `control:read` | Models |
| `providers_list` | `providers list` | `GET /providers` | `control:read` | Provider inventory |
| `route` | `route <model> [--prompt ...]` | `POST /route/preview` | `control:read` | Route preview |
| `requests` | `requests [filters]` | `GET /requests` | `control:read` | Requests |
| `observe_status` | `observe status` | `GET /observe/status` | `control:read` | Telemetry |
| `policy_status` | `policy status [--view active\|disk]` | `GET /policy/status?view=...` | `control:read` | Policy overview |
| `policy_show` | `policy show <name> [--view active\|disk]` | `GET /policy/show?name=...&view=...` | `control:read` | Named policy detail |
| `agents_list` | `agents list` | `GET /agents` | `control:read` | Catalog, without launch controls |
| `reload` | `reload` | `POST /reload` | `control:reload` | Explicit reload action |

Supporting resources are `GET /capabilities`, `GET /state`, and
`GET /operations/{request_id}?instance=...`. State requires `control:read`; operation lookup
requires `control:reload` and ownership by the same credential. Capability
discovery requires any valid control credential and discloses only its grants.

`status --requests` remains a compatibility form. Remote `policy reload`
remains unavailable in this release: its current local implementation is a full
daemon reload, and the remote client should name that operation explicitly.
`agents list --remote` continues to mean fetching the external ACP registry in
local CLI usage; reject its combination with a remote context before fetching.

### Explicit exclusions

- Agent sessions, ACP/WebSocket transport, launching/checking/installing agents,
  native harness UIs, and MCP connection diagnostics.
- Start, stop, restart, upgrade, and service-manager integration. The control
  listener dies with the daemon and cannot restart it itself.
- Config uploads/edits, provider login/logout, key management, environment
  propagation, arbitrary config reads, and remote credential administration.
- Policy compile/publish/rollback, eval submission, optimization runs, and
  changes to policy-lock files. These need artifact lineage and publication
  contracts beyond reloading an already prepared host configuration.
- Skills/workspace browsing, arbitrary log tails, request bodies, file sync,
  durable jobs, Cloud tenancy, and new MCP tools.

The useful first workflow is: prepare host files using the operator's existing
deployment workflow, inspect disk policy remotely, then explicitly reload them.
Remote editing is not implied by remote administration.

## 4. Typed actions and target resolution

Create one app-owned `ControlActionSpec` inventory. Each row contains its
canonical ID, action version, typed input/output schema functions, explicit
HTTP method/path, scope, CLI binding, and dashboard availability. Existing
shared actions reference the IDs and schemas in `bitrouter-mcp::actions::ACTIONS`;
the build/test guard rejects disagreement or dangling references. New actions
need not acquire an MCP tool solely to enter this inventory.

The inventory replaces the separate remote action-name array and centralizes
eligibility, discovery, and authorization metadata. Typed handlers and clients
remain ordinary Rust; do not introduce runtime schema execution, arbitrary
`execute(command)` RPC, shell forwarding, or generic JSON business logic.
Resource endpoints have a small explicit route inventory with their own guards.

Resolve the execution target once, before execution config, provider env, DB,
or filesystem access. The resulting local/HTTP action ports serve both CLI and
dashboard. Reading the client's context store and its selected control token is
expected; loading the client's router configuration is not. Validate
command-specific flag combinations centrally before invoking a port.

Existing MCP-shared contracts stay in `bitrouter-mcp`; new administration-only
contracts live app-side beside their action ports. Human rendering stays in
`output/reports`. Do not extract a new crate for this release. The renderer
continues to contain no network client, daemon IPC, or database access.

Preserve the three existing daemon MCP tools, but inject the same authorized,
redacted read ports used by HTTP. Control authentication must attach a validated
control caller; do not interpret `CallerAuth::default()` or a raw supplied bearer
as a remote scope grant. Keep the separate cloud backend and its profile guards
unchanged. MCP tool schemas alone do not grant access to an action.

## 5. Authentication and compatibility

Keep loopback binding, HTTPS for non-loopback client URLs, disabled redirects,
same-origin checks, bearer authentication, and `Cache-Control: no-store`.
Inference authentication and `server.skip_auth` remain unrelated.

Proposed optional configuration:

```yaml
control:
  enabled: true
  listen: 127.0.0.1:4358
  credentials:
    - id: workstation-admin
      token_env: WORKSTATION_ADMIN_CONTROL_TOKEN
      scopes: [control:read, control:reload]
```

When `credentials` is absent or empty, require the existing
`BITROUTER_CONTROL_TOKEN` and grant it `control:read`. When explicit credentials
are configured, accept only those entries; the legacy variable is not an extra
implicit credential. Require `control:read` alongside `control:reload` so an
administrator can inspect the target before and after a mutation.

Validate unique IDs, distinct token values, valid environment-variable names,
known scopes, and token length of at least 32 bytes at startup. Store only
digests in the server auth state, compare without token-dependent early exits,
and never serialize credential values. Unknown or duplicate configuration is a
startup error. Token issuance, rotation, and revocation use host-local config/env
changes plus service restart; no credential database or remote token CRUD here.

`control:read` explicitly grants host-wide operational data, including spend,
request metadata, and policy rules. It is not a personal usage scope. A narrower
usage permission can be designed later without pretending the current reports
are tenant-filtered. Existing reader tokens remain read-only but can access the
new inspection actions after upgrade; document that scope expansion.

Preserve protocol version 1 and the existing `actions: [string]` field and its
legacy names (`models`, `route_preview`). Add `action_descriptors` keyed by
canonical IDs with per-action versions and required scopes, plus
`server_instance_id`, effective grants, and advertised limits. Generate both
representations from the one inventory, with explicit legacy-name mappings.
Existing route paths and report fields retain their meanings.

New clients accept old servers for existing reads, mapping legacy action names.
They require the new descriptors and state resource before invoking reload.
Never infer write support from binary version or probe it by sending a mutation.
Cache discovery for at most 30 seconds in a long-lived dashboard; invalidate on
instance change or capability errors. Every handler still checks authorization.
Breaking report semantics require an action-version change and client gating.

## 6. Read reports and disclosure

Use additive fields or explicitly versioned reports, with source and availability
expressed as data. Existing local CLI defaults and JSON fields remain compatible;
new `--view` flags opt into the explicit policy views locally. Remote policy
commands default to `active`; local legacy policy commands without `--view`
continue to report disk state and identify that source in an additive field.
The new dashboard always requests an explicit view.

### Policy and runtime state

`active` is read from live runtime snapshots; `disk` parses the server's configured
files without activating them. Both return named policy IDs, digest, mode,
bindings, and a typed policy view. Omit raw paths. If no active policy exists,
report `not_configured`; do not substitute the disk policy. Invalid disk data
returns safe validation diagnostics while the active view remains available.
Capture active mode, bindings, and policy detail with the relevant runtime
snapshot; do not combine a live digest with startup config or freshly read disk
bindings. An active snapshot may legitimately differ from disk after a failure.

Do not use the existing arbitrary `PolicyReport.policy: Value` serialization as
an automatic remote allowlist. Define the policy detail fields that operators
need: tier targets, efforts, variants, route rules, and certificate/evidence
identifiers. Omit embedded raw evidence, paths, and extension payloads unless
explicitly included in the versioned schema.

`GET /state` reports the boot instance ID, reload generation, whether a reload
is running, last reload outcome, and live policy digest if present. A generation
identifies a reload attempt boundary, not an atomic snapshot of every subsystem.
Report `mixed` after partial application; do not label the whole router with one
successful configuration digest. Reads during a reload indicate that they may
span subsystem versions. Disk reports never change that live-state claim.

### Requests, telemetry, and agents

- Requests add `--since`, `--until` (RFC3339), `--model`, and `--provider` with
  equivalent query fields. Preserve the existing default of today since UTC
  midnight, using the server's clock;
  explicit ranges use absolute instants and include resolved bounds in the
  report. Maximum explicit window: seven days; maximum rows: 500. Both time
  bounds are required together with `since < until`, using `[since, until)`.
  Apply filters before the limit and to spend
  summaries; label any host-wide rate separately. Return `truncated` when a
  further row exists. Cursor pagination/export is deferred.
- Add a metering availability field so a failed store/query cannot look like
  successfully observed zero usage. Preserve successful fields on partial query
  failure and label unavailable components. Do not fabricate zero-cost evidence.
- Telemetry reads the daemon's observation provider. Report compile/wiring
  state, sampling and counters. Omit the socket; omit raw endpoint strings and
  header values. A feature-off server reports that state, not a client-side
  compile-time guess.
- Provider listing returns IDs, model counts, and activation state from the
  daemon's accepted config/catalog view. Activation means routing configuration,
  not a successful connectivity probe. Omit raw API bases and credential details.
- Agent listing returns IDs, catalog descriptions, and configured flags from the
  server's accepted config/catalog view. Custom entries use a neutral description, never the
  configured command/args/env. Do not claim readiness without a check, inspect
  binaries by running them, or fetch an external registry on this path.
- Preserve route-preview provenance and current limitations. The live daemon's
  route-chain answer does not become a full prompt-dependent inference
  simulation merely because the HTTP input accepts `prompt`.

Apply one remote disclosure policy before REST or MCP rendering: allowlisted
fields, no credential/env/config dumps, no socket/filesystem paths, and no raw
upstream error bodies. Request errors become bounded categorized summaries;
validation errors expose field identifiers and safe codes, not interpolated
secret-bearing source text. Test malicious token-bearing URLs, command args,
and upstream errors against this boundary. No general file or log read endpoint.

## 7. Reload contract

### Invocation and admission

`bro --context workstation reload` means: reload the server-owned source
as it exists when the server prepares the operation. It neither uploads files
nor forwards the client's environment. It does not claim to apply a previously
reviewed immutable candidate. Policy publication and a frozen dry-run/apply
workflow remain deferred.

The client fetches `/state`, generates a UUID request ID, and submits:

```json
{
  "request_id": "<uuid>",
  "expected_server_instance_id": "<boot-id>",
  "expected_generation": 12
}
```

Accept no additional fields. Under the daemon's reload coordinator: authenticate,
resolve any existing request ID, verify instance/generation, reserve bounded
operation capacity, and accept the operation. Return `202` with an operation
report and relative same-origin lookup URL. A repeated identical request from
the same credential returns the existing operation without another reload;
reuse with different input returns `409 idempotency_conflict`.

An instance mismatch or stale generation returns `409` without mutation.
Concurrent reload admission returns `409 reload_in_progress`; do not queue
unbounded work or silently refresh the generation and retry. These checks fence
other reloads, not edits made to disk by external tools.

### Execution and actual consistency guarantee

Move all reload entry points—remote HTTP, local IPC, policy reload, and SIGHUP—
through the same coordinator. Environment overrides from local IPC must be
serialized inside it, not installed before taking the reload lock. Preserve the
local feature, but exclude that input from remote requests.

Prepare and validate the candidate and its policy inputs before any live swap.
Capture the inputs used during preparation; subsequent stages must consume the
prepared objects rather than silently re-read changed files. Provider discovery
may contact configured upstreams and must have bounded timeouts. No client can
supply an upstream address through this action.

Classify changed config fields explicitly as reloadable or restart-required.
Listener/auth/control credentials, database connection configuration, process
wiring, and other startup-only settings are restart-required. Reject such a
candidate before mutation and return safe field names. Unknown changed fields
default to restart-required until their live consumer is inventoried. The
implementation must pin this classification to actual startup/reload consumers.

The coordinator covers preparation, mutation, result recording, and generation
advancement. Advance the generation once for every accepted terminal attempt,
including failed attempts; leave it unchanged for admission rejections.

Do not promise cross-subsystem atomicity. Return a result for each of the actual
reload participants: routing table, upstream timeout clients, policy table,
named policy runtime, and access-policy store. Each reports `applied`,
`unchanged`, `failed`, or `not_attempted`. Overall terminal state is:

- `succeeded`: all intended participants completed successfully.
- `failed`: preparation failed or failure occurred with no live change.
- `partially_applied`: at least one live change occurred and completion failed.

Audit each underlying mutator's own failure behavior; an `Err` after an internal
change must not be mislabeled as no change. Refactor it to report the change or
prepare it safely. In-flight inference is not drained; requests may observe
different subsystem versions during application. Do not automatically roll back
or retry a partially applied reload. The report identifies what changed and
what the operator must inspect before another explicit attempt.

### Disconnects, limits, and operation lookup

An admitted reload is owned by the daemon and continues when HTTP disconnects.
CLI/dashboard poll its operation, not repeat the POST with a new ID. Default
client wait is 30 seconds; on expiry emit `running` plus the request/instance IDs
and exit nonzero. A proposed recovery command is:

```console
bro --context workstation operations show <request-id> --instance <boot-id>
```

This resource command requires an explicit remote context and queries the
operation resource with the supplied instance ID. Register its CLI mapping in
the resource inventory; it never performs the write again. Context metadata
commands remain client-local.

Keep results in bounded memory for the current boot: at most 1,024 admitted
remote operations, retained for at least 24 hours after completion. Do not evict
unexpired entries to admit a mutation; return `503 operation_capacity` before
starting work. Advertise limits and timestamps. Clients must not replay a POST
after the reported retention deadline or against another boot instance.

Daemon restart loses operation results. A boot mismatch or expired/unknown ID
returns an explicit unknown-outcome error; it is never evidence that a reload
did not occur. The operator inspects live state before issuing a new operation.
Do not advertise durable jobs or exactly-once execution across process restarts.

The HTTP request timeout is not an execution cancellation deadline. Bound
preparation/network waits; avoid interrupting a live commit midway to satisfy a
transport timeout. Fatal daemon failure leaves an unknown outcome. No operation
cancellation endpoint in this release.

Record structured operational audit events for admission and outcome with
credential ID, action, request/instance IDs, generation, safe subsystem results,
and duration. Log rejected attempts without credentials or raw request bodies.
These events use the existing logging infrastructure; they are not a durable
operation database or compliance-grade audit ledger.

## 8. Error and client behavior

Keep the existing `{ "error": { "code", "message" } }` envelope; add safe
structured details only where defined. Use `401 unauthorized`, `403 scope_denied`,
`400 invalid_request`, `404 unsupported_action` / `operation_not_found`,
`409 stale_generation` / `server_instance_changed` / `reload_in_progress` /
`idempotency_conflict`, `410 operation_expired` when known, and
`503 operation_capacity`. Unknown operation IDs do not reveal another caller's
activity. Partial application is a terminal operation report, not a generic
HTTP 500 that invites retry.

New JSON inputs reject unknown fields, unsupported content types, and bodies
over 16 KiB; retain the existing route-preview input limit separately. Limit
operator-supplied identifiers to 256 bytes and reject invalid filter values
before querying storage. Enforce bounded report sizes and publish limits in
capabilities; never silently truncate a policy into a misleading valid report.

CLI output remains JSON by default with the existing human rendering mode. Any
failed/partial/unknown reload outcome exits nonzero. An explicit CLI reload
command authorizes the mutation; do not add an interactive prompt that breaks
automation. The dashboard presents host identity and reload scope before the
user activates its explicit reload control, then shows operation progress and
results. It must never launch a mutation from a refresh timer.

Refresh dashboard panels independently. Preserve the last successful data with
its timestamp and mark it stale after an error; distinguish unavailable,
unauthorized, failed, and loading states. A server lacking requests or telemetry
must still provide useful overview/models panels. Disable unsupported writes
with an explanation. Network/auth errors never become local data or a false
claim that the remote daemon is stopped.

## 9. Implementation sequence and ownership

| Phase | Deliverable | Exit condition |
| --- | --- | --- |
| A: contract and parity foundation | App-owned remote inventory, typed target ports, additive discovery, shared remote disclosure, independent panel refresh. | Existing remote commands/MCP reports remain compatible; socket/error disclosure and capability mismatch cases have regression coverage. |
| B: administration reads | Providers, telemetry, active/disk policy reports, passive agents, bounded request filters; CLI and dashboard consumers. | Same target/input/authority yields matching typed data; disk/live divergence and unavailable metering are visible. |
| C: reload ownership | Unified reload coordinator, input preparation, field classification, truthful participant results, boot/generation state. | Local IPC/SIGHUP concurrency and partial failure tests pass before enabling remote writes. |
| D: authorized remote reload | Named scoped credentials, reload endpoint, bounded operation retention/lookup, CLI and dashboard progress/recovery. | Disconnects and repeated requests cannot initiate a second attempt within the advertised window; no implicit write upgrade. |
| E: release acceptance | Real HTTP and isolated Docker client/server tests, CLI/skill/config docs, packaging checks. | The full read-and-reload workflow passes; exclusions remain rejected before side effects. |

Expected ownership:

- Split `remote_control.rs` into a `remote_control/` module as needed for
  contracts, inventory, authentication, HTTP server/client, and operation state;
  keep public module paths explicit and do not add re-exports.
- Keep new action implementations beside `apps/bitrouter/src/actions/` and
  renderers in `output/reports/`. Add only the ports needed by these consumers.
- `reload.rs` owns coordinated reload semantics; `daemon.rs` adapts local IPC.
  The composition root injects live policy/observation/reload ports into the
  control listener instead of treating local IPC as a remotely serializable API.
- `main.rs` handles command parsing and output; `dashboard.rs` handles effects;
  `bitrouter-tui` renders supplied state. Config schema changes belong in the SDK,
  but operator authorization and lifecycle logic remain app-owned.
- Update `docs/CLI.md` and `skills/bitrouter/` in each implementation change
  introducing flags/config/env behavior. Check agent-plugin manifests if their
  CLI wiring changes. Coordinate published prose separately in `bitrouter-docs`.

## 10. Verification and acceptance

Required automated coverage:

1. Inventory guards join canonical IDs/schemas, HTTP routes, required scopes,
   supported CLI leaves, and dashboard availability. Existing MCP/cloud profile
   guards stay intact. Legacy capability names are deliberate mappings.
2. Real HTTP tests cover missing/wrong/read-only/admin tokens; redirects and
   origin rejection; unsupported actions; old/new client-server combinations;
   bounds/content types; and secrets in every report/error channel.
3. Remote target tests arrange tempting local config, metering, sockets, and
   provider env, then prove denied/failed remote commands never consult them or
   perform local effects. Remote operation lookup is tested separately from
   client-local context metadata management.
4. Read tests exercise active/disk policy disagreement, invalid disk policy,
   unconfigured runtime, feature-off telemetry, filtered/truncated requests,
   missing versus empty metering, and custom agent command redaction.
5. Reload fault injection covers each participant before and after a live
   change; mixed state is reported correctly. Restart-only changes fail before
   mutation. Local overrides cannot race remote preparation.
6. Reload admission tests race HTTP, IPC, and SIGHUP; verify generation checks,
   same-ID deduplication, different-body conflict, capacity exhaustion, owner
   isolation, disconnect-after-admission, deadline expiry, and boot changes.
   Unknown outcomes must never cause an automatic fresh mutation.
7. Dashboard tests cover independent panel errors, stale-data labels, scope
   changes, unsupported features, explicit reload initiation, and partial results.

The maintainer selected Docker for network-boundary acceptance on 2026-09-08.
Use separate client and server containers: the client has no server configuration
or provider credentials, and the server exposes its loopback control listener
through a TLS proxy. This replaces the original physical two-machine test
environment, not the action, authorization, or failure requirements; record it
as container acceptance rather than physical-host deployment verification. Verify
every advertised read, stage a host-side policy change and observe disk/live
divergence, reload with write authority, and verify the live policy afterward.
Drop the client connection after admission and recover the same operation.
Inject a partial reload failure and verify that both clients show it accurately.
Repeat with the legacy read token and prove reload is rejected. Restart the host
and verify that old operation IDs produce an unknown outcome, not a retry.

Before code submission run the repository-required all-feature tests, clippy,
and formatting checks; run distribution checks when schemas or shipped skills
change. A documentation-only proposal needs link/diff review, not a Rust build.
Keep test evidence separate from this proposal's planned acceptance criteria.
