# RFD: remote ACP sessions for CLI and TUI clients (Phase 2)

Status: **deferred; no implementation in the HTTP-only remote-control MVP.**
Baseline: `main` at `61dd77333a91f7aab01647b0ae7d8625d3039ffe`.
Verified: 2026-09-07.

The approved first release is the read-only HTTP control plane in
[`REMOTE_CONTROL_MVP_SPEC.md`](REMOTE_CONTROL_MVP_SPEC.md). This document keeps
the researched ACP-over-WebSocket design for a later phase; none of its WSS,
remote process, workspace, permission, or reconnection requirements are part of
the MVP.

## 1. Decision and product boundary

Run the headless CLI or BitRouter interactive TUI on computer A, with the
BitRouter proxy, ACP controllers, ACP agents, workspaces, provider credentials,
and metering on computer B. Add authenticated HTTPS management and a versioned
BitRouter WebSocket transport carrying stable ACP v1 messages. Both clients
continue to use the existing action reports, ACP client, and presentation code.

There are two primary presentation modes: `run <agent>` for automation and
`tui [agent]` for people. `launch` invokes an agent's native UI; `acp serve` and
`spawn --serve` are integration mechanisms; `spawn -p` and `chat` remain
compatibility aliases. None becomes another remote UI in this release. Remote
operation is a deployment choice selected by a context, not a parallel command
hierarchy.

Those names landed with the HTTP-only MVP. The Phase 2 design remains
transport-orthogonal: remote ACP changes controller resources, policy, action
IDs, and transport, without introducing another command hierarchy.

The first release serves a **trusted operator's host**, not mutually untrusted
tenants. Session credentials authorize code execution as the configured host
user through an agent. Workspace and method allowlists prevent accidental or
API-level escapes; they are not an OS sandbox. Separate hostile workloads need
separate OS identities/containers and isolated harness storage, outside this
release. Read-only credentials can inspect explicitly granted host data without
creating agents.

The implementation is complete only when both remote headless and interactive
paths work, including bounded reconnection. Intermediate PRs may expose a
disabled-by-default preview, but remote read actions alone are not completion.

## 2. Verified starting state

The source files below, rather than earlier proposed specs' status labels, are
the baseline. The parity progress ledger records completion on the current
merged stack; some descriptions in `docs/README.md` still say proposed.

| Existing component | Verified behavior and remote gap |
| --- | --- |
| [`daemon.rs`](../apps/bitrouter/src/daemon.rs), `DaemonCommand`, `send_command` | Status, models, route preview, reload, stop, observation, and ACP lease/spend operations use local control IPC. Its owner-trusted inputs include an API principal, controller ID, and reload environment overrides; this enum is not a safe public API. |
| [`server.rs`](../crates/bitrouter-sdk/src/server.rs) | Inference is HTTP; the usual local endpoint is `127.0.0.1:4356`. A remote inference URL does not move an agent process or provide client management. |
| [`acp_cli.rs`](../apps/bitrouter/src/acp_cli.rs), `serve`, `chat`, `prompt` | Resolves local config and agent commands, applies endpoint wiring, and launches a local agent. `chat` and `acp serve` establish a local controller route/cost binding. The one-shot path must not be assumed to have equivalent attribution today. |
| `LocalControllerBinding::open` in the same file | An explicit `--base-url` disables the binding, including for an explicitly written local URL. It advertises neither route controls nor router-attributed cost. `--base-url` remains an inference endpoint option. |
| [`controller.rs`](../crates/bitrouter-sdk/src/acp/controller.rs) | One controller owns one harness connection carrying native session IDs. The controller forwards native lifecycle and callbacks; manager disconnect tears down the harness and live route state. No durable BitRouter conversation store exists. |
| [`client.rs`](../crates/bitrouter-sdk/src/acp/client.rs), `AcpClient::connect` | Already accepts `ConnectTo<Client>` instead of requiring stdio. It owns ACP handshake, permission resolution, cancellation, raw and translated updates, and route capability probing. File/terminal tooling capabilities are off. `terminal_auth` is caller-selected and must be false remotely. Update broadcasts currently filter lag errors: reliable network replay alone cannot make a lagging consumer lossless. |
| [`transport.rs`](../crates/bitrouter-sdk/src/acp/transport.rs), `AcpTransport` | Configures upstream agents over stdio only. The new client-to-host connection does not require adding remote upstream agents to this enum. |
| [`actions/mod.rs`](../crates/bitrouter-mcp/src/actions/mod.rs) | `ACTIONS` inventories CLI/MCP/TUI mappings, report schemas, effect/inverse, requirements, and `Reach`. Shared read reports/ports live here; implementation and human rendering remain app-side. |
| [`actions/session.rs`](../apps/bitrouter/src/actions/session.rs), `SessionPorts` | Bundles status/models/route ports and renders their shared reports for the TUI. Today `open` selects local implementations and `run` uses default local caller auth. |
| [`actions/status.rs`](../apps/bitrouter/src/actions/status.rs) | `DaemonStatus` ignores `CallerAuth`; spend is host-wide. Exposing this implementation under a personal-looking read scope would leak other callers' data. |
| [`actions/models.rs`](../apps/bitrouter/src/actions/models.rs), [`actions/route.rs`](../apps/bitrouter/src/actions/route.rs) | Prefer daemon answers and can fall back to local config. The models fallback includes older daemons rejecting the command. Remote failure must never cause client A's config to answer for B. Live route preview does not simulate prompt-dependent policy: retain that limitation honestly. |
| [`bitrouter-mcp/server.rs`](../crates/bitrouter-mcp/src/server.rs), `http_profile` | `/mcp-control` offers only backend-provided `status`/`list_models`. Its local unauthenticated profile refuses non-loopback; the cloud profile forwards caller credentials. Presence-only bearer middleware is not local token validation. Existing Portable-only guards must remain intact. |
| [`chat/session.rs`](../apps/bitrouter/src/chat/session.rs), [`bitrouter-tui`](../crates/bitrouter-tui/src/lib.rs) | The app drives asynchronous effects; the renderer draws. The renderer has no app dependency or async runtime. The chat driver must not acquire daemon report/storage knowledge. |

Read alongside [ACTIONS_SPEC.md](ACTIONS_SPEC.md),
[ACP_CONTROLLER_SPEC.md](ACP_CONTROLLER_SPEC.md),
[CLI_TUI_PARITY_IMPL_SPEC.md](CLI_TUI_PARITY_IMPL_SPEC.md), and the authoritative
[session skill reference](../skills/bitrouter/references/sessions.md).

### 2.1 Explicit amendments to earlier decisions

This spec narrowly supersedes ACP_CONTROLLER_SPEC §22.7's rejection of a shared
ACP supervisor: remote clients provide the new concrete consumer requiring
server-owned process lifetime. Local stdio behavior stays as specified there.

The control credential below is **management authentication**, never a new
harness/session attestation token. ACP_CONTROLLER_SPEC §22.9's reasoning still
applies: inference correlation headers are claims made by the API credential
holder. Its proposed hosted-Core route-control phase, which keeps agents on the
client, is a different topology and does not implement this requirement.

`Reach::HostBound` and `SessionBound` currently also say local-only. Amend those
comments to describe their subject and the existing MCP profile restriction;
add explicit remote-host exposure metadata in §10. Do not reclassify route
preview as Portable or widen the MCP HTTP tool set.

## 3. Goals and non-goals

Goals:

- Run `status`, `models`, `route`, `spawn -p`, and `chat` against a selected host.
- Keep provider access and tool execution on B; A needs no harness installation,
  provider credential, BitRouter server config, metering DB, or control socket.
- Preserve JSON/human report parity and headless NDJSON/text/quiet rendering.
- Preserve native session IDs, route precedence, cost provenance, permission
  decisions, cancellation, and TUI slash-command behavior.
- Survive a short network interruption without another harness, prompt, tool
  execution, permission approval, or duplicated terminal output.
- Fail visibly on capability mismatch, lost replay, expired credentials, or an
  unreachable selected host. Make the active host/workspace clear to users.

Non-goals for v1:

- File synchronization, uploading repositories, remote editor buffers, SSH/PTY
  hosting of `launch`, or client-side ACP filesystem/terminal execution.
- Cloud tenancy, cross-host agent routing, automatic load balancing, moving a
  live controller between hosts, browser clients, or an implementation of ACP v2.
- Durable BitRouter transcripts, background jobs, `--no-wait`, process-restart
  attachment, shared spectators, multiple writers, or native history browsing.
- Remote agent/package installation, credential login, arbitrary commands/env,
  provider configuration, policy publication, or daemon/service administration.
- Changing the default JSON output policy or redesigning the command palette.

## 4. Terminology and CLI/TUI UX

| Term | Meaning |
| --- | --- |
| context | Client-local named connection settings; `local` means today's behavior. |
| host | The selected BitRouter deployment on B. |
| workspace | Server-configured opaque ID mapped to a canonical directory on B. |
| controller ID | Server-generated runtime resource ID for one agent process/ACP connection; not a conversation ID or secret. |
| native session ID | Opaque harness-returned ACP `sessionId`, never replaced. |
| connection epoch | Server-issued writer generation preventing two sockets from writing after reconnect. |
| reconnect | Restoring the transport of the same running client and controller. It does not mean ACP `session/resume` or loading history. |

Proposed commands (none of these new flags exist at the baseline):

```sh
bitrouter context add workstation --endpoint https://router.example.net \
  --token-env BITROUTER_WORKSTATION_CONTROL_TOKEN --workspace bitrouter
bitrouter --context workstation status --human
bitrouter --context workstation models
bitrouter --context workstation route bitrouter/auto
bitrouter --context workstation spawn claude-acp -p 'review the changes'
bitrouter --context workstation chat claude-acp
bitrouter --endpoint https://router.example.net --control-token-env BR_CONTROL_TOKEN \
  chat codex-acp --workspace bitrouter
```

Examples above deliberately use current baseline names. If the canonical naming
proposal is accepted first, the primary examples become `--context workstation
run claude-acp -p ...` and `--context workstation tui claude-acp`; preserve the
approved compatibility behavior separately. An always-interactive `tui` would
require a usable terminal before allocation. Its pipe behavior must not be
inferred from today's `chat` fallback. This remains a choice between two
presentation modes, not a third agent-lifecycle surface.

`context add|list|show|use|remove` operates only on client connection metadata.
`add` does not switch the active context. Store contexts separately from
`bitrouter.yaml`, under the resolved client home, with owner-only permissions.
Store credential references, not token values; v1 supports an environment name
or an owner-readable token file. No secret argument on the CLI. Output redacts
credential material. `context show` does not fetch host state.

Selection order is explicit `--endpoint` or `--context` (mutually exclusive),
then `BITROUTER_CONTEXT`, then the saved current context, then `local`.
`--context local` always overrides a saved remote default. Explicit endpoints
require an explicit token reference and `--workspace` for execution; contexts
may provide defaults. `--workspace` overrides the context's workspace ID.

Resolve the target **before loading execution config or agent catalogs**. Remote
commands do not run local onboarding, provider discovery, registry refresh,
daemon auto-start, or executable checks. Their client-only needs are context,
credentials, output preferences, and explicit prompt content.

For remote session commands, reject `-c/--config`, `--base-url`, `--direct`,
arbitrary trailing args, environment overrides, raw working-directory paths,
MCP server definitions, and `--no-wait` before creating a controller. Accept
`--model` as a server-policy-validated route request. `--no-start` is an
accepted no-op remotely, with a help explanation; remote execution never
auto-starts anything. Existing local meanings remain unchanged.

Remote prompts act on the server workspace. A startup banner and TUI footer
show `workstation · bitrouter · claude-acp`; file/tool cards label server paths
as remote and do not open them as local files. Permission prompts retain that
context. `/status`, `/models`, and `/preview` refer to B. `/route` is enabled only
if both the controller binding and caller scope allow it. Remote-unavailable
commands carry a reason in `/commands`. Prompt-expansion commands come from the
server's validated chat configuration for this profile; do not silently merge
client `bitrouter.yaml` templates. Expansion remains client-side plain text,
with the existing collision checks, and executes only on user submission.

Headless stdout remains the existing typed report or NDJSON stream. Connection
progress goes to stderr. Add optional `host`, `workspace`, and `controller_id`
fields to the existing first `session` NDJSON record; keep `session_id` native
and the terminal `result` semantics. Transport ACKs, replay frames, tokens, and
ping/pong never appear in stdout. Interactive reconnect freezes new submissions,
keeps the draft, and shows `Reconnecting…`; success restores the same session.

## 5. Architecture and listener

```text
Computer A                                Computer B
CLI output / TUI renderer                 bitrouter serve
    |                                          |
    +-- shared action ports -- HTTPS ----------+-- remote control API
    |                                          |      |
    +-- AcpClient ----------- WSS -------------+-- controller supervisor
                                               |      |
                                               |   ACP controller -- stdio -- agent
                                               |      |                         |
                                               |   local route/cost ports        |
                                               |                                |
                                               +-- inference proxy <--- loopback+
                                                      |
                                                   providers
```

Three logical planes, with separate authorization:

| Plane | Transport | Owner and responsibility |
| --- | --- | --- |
| inference | Existing model HTTP routes, normally `127.0.0.1:4356` on B | SDK server/pipeline; normal API credentials and metering. |
| host control | `/control/v1/*` over HTTPS | App-owned actions, capability discovery, workspace/agent discovery, controller allocation/status/termination/logs. |
| ACP session | `/control/v1/controllers/{id}/acp` over WSS | BitRouter reliable message transport, then existing ACP controller and client. |

Add an app-owned optional control listener, configured separately from inference.
Recommended v1 deployment: loopback `127.0.0.1:4358` behind an operator-managed
TLS reverse proxy on B; both listener and hostname are configurable, and `4358`
is a **proposed** default. Keep `4356` and existing MCP binds unchanged.
`bitrouter serve` supervises this listener alongside inference and the local
control socket. It does not start a second daemon or install a service.

Native control TLS is deferred. The v1 listener refuses non-loopback addresses;
the reverse proxy forwards the control path and WebSocket upgrades over
loopback. The client requires HTTPS/WSS except explicit loopback HTTP for
development or an operator-established tunnel. No general `--insecure` option.
Configure the external HTTPS origin explicitly; never derive trusted URLs or
authorization from client-supplied `Host`/forwarding headers. Trust only configured
proxy addresses for client-IP audit data. Application bearer validation remains
mandatory behind the proxy, including on loopback; inference `skip_auth` cannot
disable it. The proxy must not log authorization, attach tickets, or bodies.

Set `control.enabled: false` by default. An enabled listener with missing
credentials, invalid allowlists, or an invalid bind fails startup visibly rather
than silently omitting security. If the control listener fails after startup,
stop accepting controllers and report unhealthy; do not affect in-flight model
requests merely to hide that failure. Normal daemon shutdown drains controllers
before flushing telemetry. A supervisor such as launchd/systemd remains
responsible for starting the daemon when it is down.

## 6. HTTPS contract

All routes are authenticated before disclosing deployment details. Prefix v1
paths with a reverse-proxy path prefix only through the configured endpoint;
clients resolve returned relative links on the same origin. Reject redirects
for authenticated API calls and never forward credentials across origins.
Successful responses use `application/json`; sensitive responses use
`Cache-Control: no-store`. No cookie authentication or browser CORS in v1.

### 6.1 Capability and version handshake

`GET /control/v1/capabilities` returns a typed descriptor before any agent starts:

```json
{
  "api_version": "1.0",
  "server_version": "<bitrouter semver>",
  "server_instance_id": "<random per daemon boot>",
  "deployment_id": "workstation",
  "actions": {
    "status": {"version": 1},
    "list_models": {"version": 1},
    "route": {"version": 1}
  },
  "session": {
    "transport": "bitrouter.acp.v1",
    "acp_versions": [1],
    "reconnect": true,
    "reconnect_grace_seconds": 60,
    "native_history": false,
    "client_filesystem": false,
    "client_terminal": false,
    "terminal_auth": false
  },
  "limits": {"max_frame_bytes": 1048576, "max_controllers": 4},
  "scopes": ["control:read", "session:create", "session:interact", "route:write"]
}
```

Return only granted actions/features, plus enough denied-feature reasons for the
client command inventory. The negotiated descriptor never replaces ACP's own
`initialize`; both layers must succeed. Compare API major, per-action version,
transport version, and actual capabilities, not exact binary semver. Additive
minor fields are ignored if unknown; incompatible required semantics get a new
major/action version. No controller on failed negotiation.

Unknown control endpoint/HTML response means `remote_control_unavailable`, with
a hint to enable/update the selected host. Unsupported versions are
`protocol_mismatch`. A timeout means `remote_unreachable`, not `running: false`:
unreachability cannot establish that B's daemon stopped. If an authenticated
control service can positively observe its inference component stopped, it may
return the normal `StatusReport` with `running: false`.

### 6.2 Typed action dispatch

`POST /control/v1/actions/{action_id}` accepts that action's typed input and
returns the **unwrapped existing report JSON**. For v1:

| Action ID | Input | Result |
| --- | --- | --- |
| `status` | `{}` | `StatusReport` |
| `list_models` | `{}` | `ModelsReport`; the existing provider filter runs after decoding on all clients. |
| `route` | Existing `RouteInput` | `RouteReport`; preserve `resolved_via` and live-preview limitations. |

Only explicit rows are dispatchable. Unknown fields in security-sensitive
requests are rejected; no arbitrary command string, `DaemonCommand`, config
path, principal, socket, or environment map can be submitted. Execution occurs
through the same app-side builders used locally, over B's injected runtime
state. Reuse report construction and policy logic; do not have handlers execute
the CLI as a subprocess or scrape human output.

For `control:read`, status's current host-wide spend is omitted. The server
adds it only for `control:usage:read`, explicitly a host-wide grant, using the
same report type. Local trusted CLI continues to include it. Full parity is
tested under equivalent subject and authorization; a restricted remote caller
is not falsely promised the host owner's report bytes. Models and route preview
describe this host's routable catalog, not a user's cloud account. Server socket
paths are omitted remotely; `pid` and `listen` remain host facts if included.
No client reads B's metering database. Remote handlers do not fall back to A's
files; internal runtime/version failures are typed errors.

### 6.3 Discovery and controllers

| Method and path | Contract |
| --- | --- |
| `GET /control/v1/workspaces` | Allowed opaque IDs and display names; canonical paths only where needed for session setup. No filesystem traversal endpoint. |
| `GET /control/v1/agents?workspace={id}` | Allowed configured agents, installed/readiness status, safe diagnostic codes, supported remote profile. Uses B's catalog/check logic. Does not install or run arbitrary checks. |
| `POST /control/v1/controllers` | Allocate a controller reservation. Require `Idempotency-Key` and create/interact scopes. |
| `GET /control/v1/controllers/{id}` | Owner-visible runtime state, server instance, agent/workspace, failure code, expiry, and native IDs already created on that controller. No transcript. |
| `POST /control/v1/controllers/{id}/attach` | Mint a 30-second single-use attach ticket for its owner, bound to current credential and the requested connection epoch; require the controller's resume secret. |
| `GET /control/v1/controllers/{id}/acp` | WebSocket upgrade; consumes attach ticket and establishes/resumes the logical connection. |
| `DELETE /control/v1/controllers/{id}` | Idempotent terminal shutdown request, `202` while draining; poll owner status if needed. Never deletes native history or workspace files. |
| `GET /control/v1/controllers/{id}/logs?cursor=...&limit=...` | Owner-scoped, bounded, redacted diagnostic records; requires `session:logs:read`. No caller-supplied path. |

Example allocation body:

```json
{
  "agent": "claude-acp",
  "workspace": "bitrouter",
  "mode": "interactive",
  "model": "bitrouter/auto",
  "permission_policy": {"mode": "interactive"}
}
```

`mode` is `interactive` or `headless`; headless policy uses the existing policy
parser/types serialized as data, with deny-all as default. Requested policy is
intersected with the host's allowed policy. Validate before allocation; the
server returns the effective policy. Allocation returns `201` with ID, state
`allocated`, selected workspace's canonical cwd, effective policy, server
instance, relative attach/status links, and a random 256-bit `resume_token`.
This secret is held only in the running client's memory and the server's
protected allocation response/digest state; it is never printed in report or
NDJSON output. It expires with the controller. Allocation does not yet spawn
an agent.
First authenticated attachment starts the process exactly once; ACP initialize
then determines actual harness capabilities/readiness. Expire unused
reservations after 30 seconds.

Idempotency keys are scoped to credential ID and a canonical request digest;
same key/body returns the original allocation, different body is `409`.
Retain the mapping/tombstone for 10 minutes or the controller's live lifetime,
whichever is longer. It is in-memory, not crash-durable. A boot change means an
uncertain creation must be reported; the client cannot automatically allocate
and resubmit under a new boot after any execution may have occurred.

There is no separate HTTP one-shot prompt runner in v1. Remote `spawn -p` drives
the same remote ACP transport as `chat`, then performs the existing NDJSON
translation on A. This keeps permission, cancellation, and reconnect behavior
shared instead of building another stream/session protocol.

### 6.4 Errors

Define an app-owned `RemoteError` with a stable `code`, HTTP status, safe message,
retryability, and request ID. Its JSON body preserves the SDK `ErrorEnvelope`
shape (`error.kind/message/context/hint`) and adds `error.code`,
`error.request_id`, and `error.retryable` in this remote wire type. Map it to the
ordinary CLI envelope/human renderer; retain `code` in remote machine output.
Do not grow inference `ErrorKind` with every controller/transport condition.

| HTTP status | Examples |
| --- | --- |
| `400` / `413` / `415` | `invalid_request`, `frame_too_large`, unsupported content type |
| `401` / `403` | `invalid_control_credential`, `scope_denied`, `workspace_denied` |
| `404` | Missing or non-owned controller, unknown action/agent; avoid an ownership oracle. |
| `409` | `writer_attached`, `idempotency_conflict`, `protocol_mismatch` |
| `410` | Owner-visible `controller_expired`, `replay_unavailable`, `server_restarted` when a retained tombstone establishes it. |
| `429` | `controller_limit`, request/frame rate limit, with `Retry-After` |
| `502` / `503` / `504` | `agent_start_failed`, `agent_auth_required`, `control_not_ready`, `agent_timeout` as appropriate |

Unknown boot resources may be `404`; the client recognizes a restart by the
fresh handshake instance ID. Errors before WebSocket upgrade use HTTP; errors
after it use a transport `error` frame and close. Harness-authored ACP errors
retain their JSON-RPC shape, inside the permitted profile. Neither HTTP `202`
nor a transport ACK means a prompt or tool succeeded.

## 7. ACP transport, reliability, and lifecycle

### 7.1 Versioned custom transport

As checked on 2026-09-07, ACP's
[v1 transport documentation](https://agentclientprotocol.com/protocol/v1/transports)
permits custom bidirectional transports preserving JSON-RPC and lifecycle.
The [HTTP/WebSocket transport RFD](https://agentclientprotocol.com/rfds/streamable-http-websocket-transport)
is active, and [ACP v2](https://agentclientprotocol.com/rfds/v2/overview) remains
a proposal. The wire below is **BitRouter-specific**, not a claim of compliance
with either draft. It can later gain a standard transport sibling after pinned
SDK/adapter conformance testing; do not change ACP versions as a side effect.

Use WebSocket subprotocol `bitrouter.acp.v1`. Native clients authenticate the
upgrade with their ordinary control bearer plus a `Bitrouter-Attach-Ticket`
header, never query parameters or a token in the subprotocol. `attach` requests
carry the current writer epoch; an initial attachment has none. A normal second
writer is `409`. Resume of the same logical connection atomically increments
the epoch and fences the old socket before accepting any more writes. Require
credential ownership, the resume secret when issuing a ticket, and the
single-use ticket on upgrade, not merely the public ID or observed epoch.
Send the resume secret in a redacted `Bitrouter-Resume-Token` header to `attach`;
it never grants access without the live control credential. The initial
allocation idempotency cache retains its original secret response only for its
specified lifetime; do not persist it or expose it through controller status.

WebSocket text messages contain transport envelopes:

```json
{"type":"hello","server_instance_id":"boot-1","epoch":2,"received_seq":18}
{"type":"resume","received_seq":51}
{"type":"message","seq":19,"payload":{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"sessionId":"native-id","prompt":[{"type":"text","text":"review"}]}}}
{"type":"ack","seq":19}
{"type":"error","code":"replay_unavailable","message":"Reconnect window was exceeded"}
```

`hello` is server-to-client; `resume` is the client's acknowledgement position
for the server stream (zero on a fresh connection). These must be exchanged
before application messages. Initial ACP `initialize` occurs inside `message`
frames after this exchange; reconnect does **not** repeat it. Sequence numbers
start at 1 independently in each direction and cover requests, responses, and
notifications. `payload` retains the complete permitted JSON-RPC message,
including opaque IDs, `_meta`, and update payloads. Reject batches and binary
messages in v1; cap each decoded text message at 1 MiB, with negotiated limits
checked before allocation or large prompt upload.

The reconnect adapter implements the existing `ConnectTo` seam using a stable
in-process channel on each side. Socket loss does not close that channel while
the grace interval is live. Keep ACP runtime state and outstanding request
correlations alive. Do not reconnect by constructing another `AcpClient`, since
that would reinitialize the harness and lose pending response/permission state.

### 7.2 Delivery rules

- Retain every outbound message until cumulatively acknowledged. An ACK means
  the receiver accepted the message into its live ordered dispatch queue, not
  that the harness executed it. Queue ownership survives socket replacement.
- Deliver a given sequence once to ACP; duplicates are acknowledged and never
  redispatched. A gap or a sequence beyond the sender's known maximum is a
  protocol error. Sequence/digest conflicts terminate the connection.
- On resume, exchange both received positions, resend only unacknowledged
  messages in order, and then allow new messages. Server epochs reject stale
  socket traffic, including delayed permission responses and ACKs.
- Retain the same client object, queues, and acknowledged positions only in
  memory. There is no exactly-once guarantee across either process restarting.
  Report uncertainty and require an explicit new invocation; never replay a
  user prompt as a new JSON-RPC request to “recover.”
- Bound unacknowledged replay per direction to 8 MiB and 4096 messages per
  controller (first limit wins), plus a global 64 MiB default budget. On overflow
  fail with `replay_unavailable` and drain/terminate; never silently skip chunks
  or permissions. Disable WebSocket compression in v1 to simplify size bounds.
- Ping every 15 seconds, declare connection loss after 30 seconds without
  liveness, and allow a further 60-second reconnect grace from that detection.
  The process may therefore continue for up to 90 seconds after a silent link
  failure. Retry with jittered 0.5/1/2/4/8-second delays capped by the deadline.
  A clean explicit shutdown skips grace. Server-reported deadlines govern.
- A blocked output consumer is also bounded. Replace silent broadcast lag on
  remote render paths with a surfaced terminal loss error or use a bounded
  lossless subscription. Transport ACK tests alone cannot prove NDJSON/TUI
  fidelity. Never let a slow stdout pipe allocate unlimited memory.

All limits are server-configurable downward or within documented safe bounds.
Negotiate actual values; clients cannot enlarge them. Test clock behavior with
a fake clock, not minute-long sleeps.

### 7.3 Runtime state and ownership

```text
allocated -- first attach --> starting -- initialize --> attached
    |                            |                         |
    +-- expire ------------------+-- failure --> draining   +-- socket loss --> detached
                                                    ^                         |
                                                    |           resume <-----+
                                                    |                         |
                                                    +-- expiry/overflow/revoke+
                                                    |
                         attached -- explicit delete/shutdown --> draining --> closed
```

The supervisor owns the agent process, route namespace, diagnostic sink,
replay buffers, allowed native IDs, and one active writer. Runtime records bind
credential ID, logical principal, allowed agent/workspace, effective policy,
server boot, and timestamps. Resource IDs are unpredictable but never substitute
for authorization. In v1 a different credential for the same logical principal
does not take over an existing controller; token rotation drains its old live
controllers unless a future explicit transfer feature is implemented.

V1 supports one new native session per remote controller. Keep that actual ID
in a live membership set for authorization, not a shadow persistent catalog.
Additional `session/new` is refused. Model/route/permission requests must belong
to that ID or its expected pending creation flow. Neither a client-supplied
native ID nor an ID read from another controller grants access.

Remote profile capability composition is the intersection of harness support,
server policy, client support, and caller scope. Suppress and reject native
`list`, `load`, `resume`, `fork`, and `delete` in v1; unscoped harness histories
can contain other workspaces' conversations. Permit `close` for the owned live
session when the harness supports it. Do not change local transparent-controller
behavior. The runtime ID is separate from every ACP `sessionId`.

### 7.4 Remote ACP policy boundary

Validate security-relevant ACP fields server-side before forwarding:

- `session/new` cwd must equal the canonical workspace selected at allocation;
  the client obtains that path from the server. Reject additional workspace
  roots and client-supplied MCP server definitions. Server-configured MCP
  integrations belong to the selected profile and are applied on B.
- Accept known session prompt, cancel, config-option, route, and permission
  messages only for the owned native session. A server registry explicitly
  allows safe config option IDs; do not allow toggling a harness's approval
  mode, sandbox, executable, auth, or provider endpoint through config options.
- Deny unknown client-originated extension methods by default. Allow the
  existing `_bitrouter/route/*` extensions under their scope and advertised
  methods. Preserve unknown metadata/content on otherwise allowed messages;
  preserve safe agent notifications without interpreting them as authority.
- Force filesystem/terminal tooling capabilities and terminal authentication
  off even if a modified client advertises them. Unsupported callbacks receive
  method-not-found/cancelled as the protocol requires. They never execute on A.
- Agent login is provisioned out of band on B. An auth-required result names the
  host/agent and instructs the operator to authenticate there. No terminal
  relaunch or credential login flow starts on A. Remote `authenticate`/`logout`
  are unavailable in v1 because those could mutate the host's shared identity.

These restrictions are an intentional remote profile, not a transparent raw
ACP relay. They must be tested with malicious direct WebSocket clients as well
as ordinary CLI flags. Agent output remains untrusted terminal content; keep
existing escaping and add control-sequence/link tests at the remote boundary.

ACP v1 file callbacks operate in the client's environment, and terminal auth
requires reproducing the configured agent invocation. Those semantics are why
these capabilities cannot be advertised by A for B's workspace. See the official
[filesystem](https://agentclientprotocol.com/protocol/v1/file-system) and
[authentication](https://agentclientprotocol.com/protocol/v1/authentication)
contracts.

### 7.5 Permissions, cancellation, and shutdown

Keep existing interactive/headless permission presentation on A. The server
also enforces the allocation's effective policy ceiling: a client cannot approve
an option that server policy denies. `--approve-all` requires
`session:approve-all` and an explicitly allowed host profile. Existing deny-all,
approve-reads, and per-tool policy parsing are reused; classification is not a
substitute for sandboxing an untrusted harness.

Pending permission requests remain open during a recoverable gap and are
replayed exactly once to the retained client. No disconnect, absent response,
timeout, unknown option, or dropped UI prompt becomes approval. At expiry,
revocation, explicit cancellation, or terminal shutdown, resolve unanswered
permissions as cancelled/denied before teardown. Recheck policy and current
credential validity when consuming a permission decision.

`session/cancel` remains cooperative. Ctrl-C while attached sends it and waits
the existing three-second turn grace; failure to settle triggers controller
shutdown rather than reporting success. While detached, mark cancellation
pending, stop accepting new work, and attempt owner-authenticated `DELETE` for
the whole controller over HTTPS; if reachable, the server performs cancellation
and teardown. If neither channel is reachable, the client must say cancellation
is unconfirmed and the server lease/deadline bounds continued execution.

The server enforces requested headless turn timeouts as well as a configured
maximum turn duration, so a vanished client does not remove the deadline.
Controller shutdown marks draining, rejects new messages, denies permissions,
cancels active turns, waits three seconds, terminates the child/process group,
waits up to two more seconds, and kills remaining owned processes. Reap children,
revoke leases, release capacity, and close channels in all error paths. Make the
cleanup idempotent and explicit about OS process-tree limitations. Killing a
controller never means deleting harness-owned conversation storage.

## 8. Authentication, host policy, route binding, and logs

### 8.1 Credentials and scopes

Control bearer tokens must be independently issued and validated. Ordinary
inference keys (`brvk_*`, cloud API keys, or `skip_auth`) never authorize
management. A control token never goes in the harness environment, endpoint
plan, ACP metadata, model request, trace, or diagnostic output.

Propose local-only `bitrouter control token create|list|revoke` commands. Create
uses OS cryptographic randomness (at least 256 bits), writes the bearer only to
an explicit owner-only output file, and returns non-secret credential metadata.
Server storage keeps a digest, credential ID, principal ID, expiry, scopes, and
agent/workspace allowlists; compare digests without timing-dependent equality.
List reveals metadata only. Revoke is effective on the running daemon through
local IPC and drains controllers/tickets for that credential. Tokens have a
required expiry; recommended default is 30 days. Provisioning runs on B, and
moving a token to A remains an operator task, not an inference-auth shortcut.

| Scope | Grants |
| --- | --- |
| `control:read` | Host status without spend, model catalog, route preview, allowed agent/workspace discovery, capabilities. |
| `control:usage:read` | Host-wide spend and, once implemented, request/spend history. This is not “my usage.” |
| `session:create` | Allocate agents within credential/profile allowlists and quotas; requires `session:interact` for the v1 attached workflow. |
| `session:interact` | Attach, prompt, permitted config changes, normal permission decisions, cancel, and terminate this credential's controllers. |
| `route:write` | Owned live session route set/reset, additionally constrained by normal inference route policy. |
| `session:approve-all` | Request a blanket-approval headless policy if the host also allows it. |
| `session:logs:read` | Retrieve diagnostics for this credential's controllers. |

Administrative restart/reload scopes are deliberately not implemented. Add
`admin:reload` only with a later endpoint that excludes the local reload command's
arbitrary client environment overlay. Starting an unreachable daemon remains
out of band, regardless of scope.

Validate credentials on every HTTP request, attach, and session operation. A
server timer also drains on expiry/revocation while sockets are idle or busy.
Malformed or missing auth fails before allocating buffers, launching processes,
or looking up non-public workspace metadata. Apply request/connection limits
before expensive parsing. Reject any browser `Origin` in v1, since browsers are
not clients of this API; never use cookies as bearer substitutes.

### 8.2 Workspace and agent profiles

Proposed config lives under `control` with `enabled`, `listen`, `public_origin`,
credential-store path, limits, and named workspace/agent profiles. Each workspace
maps an opaque ID to a canonical existing directory and a set of allowed agents.
Each agent resolves to an installed exact adapter command already configured on
B, approved model selectors, safe config options, permitted MCP integrations,
permission-policy ceiling, and environment allowlist. No remote runtime package
download/install as a side effect of discovery or launch. Preflight must identify
missing pinned artifacts and return actionable host setup diagnostics.

Canonicalize and validate workspace/config paths on B at startup and allocation;
reject symlink changes outside the approved root, traversal, and additional
client paths. Workspaces are selection constraints, not a filesystem sandbox:
an agent executing shell commands as the host user can access other user-readable
paths and credentials. The host operator explicitly trusts holders of execution
tokens with that OS-level power. Non-owner tenancy must add real isolation first.

Launch with a minimal explicit environment needed by the approved adapter:
required runtime/PATH/home settings, selected harness auth dependencies, and the
normal server-generated inference endpoint plan. Strip remote control tokens,
attach tickets, unrelated cloud credentials, and parent process secrets. Never
accept arbitrary env/command/cwd from A. Prevent diagnostics from reporting
resolved secret values when a referenced variable is missing or malformed.

Recommended default quotas: four live controllers per credential, sixteen per
host, one native session and one active prompt per controller, 30-second
allocation deadline, 30-minute idle timeout, and one-hour maximum turn timeout.
Server config can adjust these. No FIFO prompt queue: concurrent prompts are
refused; a disconnected prompt is not replaced by a queued one.

### 8.3 Route leases and cost

Launch B's agent through the existing canonical endpoint plan using B's normal
inference credential and loopback proxy URL. The server chooses the inference
principal; no field from A can select or forge it. Bind controller ID, principal,
route-control backend, and session-cost backend on B exactly as the local
`LocalControllerBinding` does. Remote one-shot gets this binding too.

Management credentials may map to separately provisioned normal inference keys
where distinct billing/route principals are wanted. Do not pretend distinct
control tokens create inference isolation under `skip_auth: true`: that remains
the shared `local` principal. This is acceptable for the trusted-host v1 profile,
and must be documented as such. Do not issue a new attestation credential or
claim controller/native session headers are cryptographic evidence.

Capability advertisement intersects the live binding with route scopes. A
credential lacking `route:write` can receive permitted route-list state but no
set/reset methods, so the existing picker gates show a precise unavailable
reason. Validate every mutation on B. Preserve existing route precedence:
explicit caller routes/presets and Responses continuation pins remain stronger
than session leases. No global default or policy file changes through `/route`.

A brief transport detach retains the live controller's lease; it is not the
manager-channel disconnect. Keep any required lease renewal alive on B, bounded
by controller lifetime. Expiry, final disconnect, shutdown, reset, revocation,
and successful native close remove leases through the existing cleanup path;
daemon restart naturally drops them. No reconnect reacquires a different route
without a visible failure or explicit user action.

Cost retains the current contract: decorate harness-emitted `usage_update` with
router-attributed cost and its provenance marker only when the existing binding
has evidence. Do not synthesize ACP usage updates or convert unpriced/absent
cost into zero. Server-local metering queries supply evidence; A does not estimate
cost from chunks. Graceful one-shot completion may query a separate existing
report for its terminal summary, but cannot label incomplete settlement as final.

### 8.4 Diagnostics and observability

Separate operational audit events from opt-in content capture. Record credential
ID (never bearer), host boot, controller ID, workspace/agent IDs, action/operation,
outcome, request ID, and time for creation, attachment, permission resolution,
route changes, cancellation, credential revocation, and cleanup. Treat raw
session/controller IDs as log/span attributes, never aggregate metric labels.

Use bounded per-controller diagnostics owned by the supervisor; the existing
single process/session log pointer is insufficient for concurrent controllers.
Return structured, redacted records with an opaque cursor; max page 64 KiB,
max retained diagnostics 5 MiB/controller, and 24-hour expiry by default. Closed
controller tombstones and log ownership metadata share that expiry. Permission
decisions include option/outcome identifiers, not raw prompt/tool arguments.
Do not log prompts, file contents, thought chunks, environment maps, auth
headers, or provider endpoint credentials by default. Redaction cannot make
arbitrary agent stderr safe by assumption: remote log export needs explicit
sanitization and tests, while detailed local capture stays opt-in and protected.

Metrics cover active/detached/draining controllers, start latency, reconnect
attempts/outcomes, replay occupancy/overflow, denied scopes, frame limits,
permission timeouts, cancellation latency, orphan cleanup, and loopback
inference errors. Keep transport events separate from ACP turn events to avoid
double-counting turns or spend after replay. Health reports listener readiness
and supervisor capacity without leaking a session inventory publicly.

## 9. Command and action exposure matrix

“Local” retains current behavior. “Remote v1” describes the completed milestone,
not every intermediate PR. Every unlisted remote command is refused with
`unsupported_remote_command` before local config, environment, or filesystem
mutation; never silently switch the selected context.

| Command/action | Local | Remote v1 | Notes |
| --- | --- | --- | --- |
| `status` / `/status` | Yes | Yes | B's liveness; spend only with explicit host-wide usage grant. |
| `models [provider]` / `/models` | Yes | Yes | B's catalog, same filter/report. |
| `route <model>` / `/preview` | Yes | Yes | Shared read-only preview; does not run inference. |
| `chat <agent>` | Yes | Yes | Existing BitRouter TUI, server agent/workspace. |
| Piped `chat` | Yes | Yes | Existing plain/headless behavior over the same remote client. |
| `spawn <agent> -p` / `acp prompt` | Yes | Yes | Same transport and NDJSON/text/quiet; deny-all default. |
| `/commands` / `acp commands` | Yes | Yes | TUI uses its connection; standalone command opens a fresh remote controller and closes it. Requires create/interact. |
| `/route`, `/route reset` | Capability-gated | Capability/scope-gated | Owned live session only; no new cross-terminal mutation command. |
| `agents list`, `agents check` | Yes | Yes | B's allowed/preinstalled entries and safe readiness diagnostics. |
| `status --requests`, detailed spend history | Yes | Deferred | Add shared typed input/report and usage scope before exposing. |
| `providers list`, `skills list/get`, `observe status` | Existing leaves only | Deferred | Explicit future inventory entries and redaction review, not automatic reach-based exposure. |
| `spawn --serve` / `acp serve` | Yes | Deferred | A later local-stdio/remote-ACP bridge must document the constrained profile. |
| `launch` | Native UI | No | No remote PTY feature. |
| `start`, `serve`, `stop`, `restart`, `reload` | Yes | No | Service supervision out of band; remote start cannot reach a stopped service. |
| `init`, `config`, provider login/logout, keys, update, installs | Yes | No | No forwarding of local administration or secret-bearing env. |
| `cloud ...` | Existing Cloud API | No context forwarding | Use explicit `--context local`; Cloud's own endpoint/credentials are a different target. |
| `context ...` | Client-local | Client-local | Explicit metadata management, not an implicit target fallback. |
| `control token ...` | Host-local | No | Run on B using local authority. |

## 10. Code/module plan and invariants

Keep runtime transports in the app for this first real consumer. No new crate
is required. The app already depends on both action and ACP contracts. A future
second consumer can justify extracting a reusable remote client crate; do not
move types speculatively or add HTTP dependencies to the renderer.

| File/module | Proposed responsibility |
| --- | --- |
| `apps/bitrouter/src/context.rs` (new) | Parse/store client contexts, resolve target and credential references before host config. |
| `apps/bitrouter/src/remote/mod.rs`, `protocol.rs` (new) | Public-within-app typed capability, controller, policy, error, and frame contracts. No re-export pattern. |
| `remote/client.rs` (new) | HTTPS requests, TLS/origin checks, version negotiation, typed remote implementations of existing action ports. |
| `remote/server.rs`, `auth.rs` (new) | Explicit HTTP routing, independent auth/scopes, discovery, input bounds, action-port injection. |
| `remote/supervisor.rs`, `policy.rs`, `logs.rs` (new) | Reservations/controller ownership, bounded lifetimes, remote ACP profile enforcement, cleanup, diagnostics. |
| `remote/transport.rs` (new) | WebSocket framing, sequencing, ACK/deduplication, epoch fencing, reconnect, `ConnectTo` channel bridge. |
| `apps/bitrouter/src/main.rs`, `lib.rs` | Target-first CLI dispatch; app-owned control listener composition/shutdown; local-only context/token commands. |
| `apps/bitrouter/src/acp_cli.rs` | Extract the currently combined launch/controller construction into reusable local host preparation; choose remote connection without local harness/config loading. Reuse one-shot rendering/policy code. |
| `apps/bitrouter/src/actions/session.rs` | Add constructor from injected ports; preserve `open` for local use. Build offered commands from effective remote/ACP capabilities. |
| `apps/bitrouter/src/actions/status.rs`, `models.rs`, `route.rs` | Share report builders with injected live server ports; make fallback policy explicit. Scope/redact status at the action boundary. |
| `apps/bitrouter/src/daemon.rs` | Keep owner-trusted IPC; add local token-revocation notification if needed, not public enum serialization. Local protocol capability support may be added here. |
| `crates/bitrouter-mcp/src/actions/mod.rs` | Add explicit remote action exposure metadata and amend `Reach` comments; no remote runtime or app dependency. |
| `crates/bitrouter-sdk/src/acp/client.rs` | Only necessary transport-facing reliability/subscription and capability adjustments; keep ACP behavior centralized. |
| `crates/bitrouter-sdk/src/acp/controller.rs` | Preserve default transparent local behavior. Add a narrow policy hook only if the app transport boundary cannot enforce before forwarding. Do not bake HTTP/auth/context into the SDK. |
| `crates/bitrouter-sdk/src/config/mod.rs` | Serializable control config consumed by the app; validate deployment/auth/path policy in app code. |
| `crates/bitrouter-tui/src/machine.rs` and renderer modules | Plain connection-status/remote-location data and display; no networking, daemon ports, DB, async runtime, or app dependency. |

### 10.1 ACTIONS integration

Extend each existing `ActionSpec` with `remote_host: Option<RemoteActionSpec>`
containing required scopes, input schema, report/action version, and subject
(`Host` or `LiveSession`). The three remote HTTP action handlers reference rows
by ID and deserialize the corresponding existing typed input. `None` means
explicitly unavailable, not permission inferred from `Reach`.

`status`, `list_models`, and `route` get HTTP exposure. `commands` and route
set/reset remain on the session channel; do not add JSON HTTP report schemas to
schema-less route mutations merely to satisfy a new table. Their remote session
permission metadata can be read by the remote policy map referencing the same
row IDs. Discovery/controller endpoints are resource APIs with their own typed
route inventory, not fake reversible actions. In particular controller creation
is not admitted as `Effect::Write { inverse }`: termination cannot undo executed
tools, so it does not satisfy that invariant.

Agent list/check and future request history acquire shared input/report types
when their second transport actually lands, with CLI/MCP fields reflecting real
surfaces only. The existing MCP tool-set assertions and Portable-only G6 guard
stay unchanged. New guards compare remote routes against their own explicit
profile. Clarify docs that `Reach` is no longer a universal network exposure
rule; do not make multiple inventories silently disagree about a route.

Keep G1–G5 (TUI inventory, complete mappings, inverse pairs, no daemon reports in
chat driver, palette-free action rendering), schema parity, and MCP multitenant
caller forwarding. The remote client holds its transport credential privately;
do not overload MCP `CallerAuth` with control token semantics or modify the
cloud backend to support this deployment.

## 11. Compatibility, implementation PRs, and rollout

Local mode remains the default with no migration. Existing `--base-url` keeps
its inference-only meaning and limitation; remote contexts do not reinterpret
it. Context-selected unsupported commands fail explicitly. An old client talks
to its old local interfaces; an old host returns a useful capability error to a
new remote client. A newer host advertises only supported versioned surfaces.
Do not loosen exact maintained ACP adapter pins to make remote startup succeed.

Local IPC remains independently versioned. Add a capability probe before relying
on newer commands where needed; classify an older daemon's rejection as version
skew rather than “no daemon answered.” Legacy local config fallback can remain,
but report its actual reason. No silent compatibility fallback is allowed for
remote execution, scope checks, or replay. A server boot change invalidates
controllers, tickets, and replay positions; never reconnect to a new process
using only a native session ID.

Implement in reviewable dependency order:

Before landing CLI-facing PRs, align the entry-point spellings with the UI naming
decision. If that decision is still pending, implement against current names
with both modes calling the same target/connection selection functions; no
protocol work waits on the spelling choice. Do not add a separate `remote run`,
`remote chat`, or agent-lifecycle command family. If `run`/`tui` lands during
this sequence, adapt their shared dispatch once and test approved aliases; keep
renaming commits separate from transport/security implementation.

| PR | Work | Done when |
| --- | --- | --- |
| 1. Contracts and target resolution | Context store/flags, capability/error/controller DTOs, remote exposure inventory and guards; reject unsupported combinations. Update CLI/skill docs for landed flags. | Remote selection reaches a typed unsupported/capability result without touching local execution config, catalog, daemon, or provider credentials. Existing local CLI output unchanged. |
| 2. Authenticated host read service | Independent listener, credential creation/revocation, TLS-proxy deployment config, allowlists, capabilities, status/models/route ports and HTTP adapters. | Auth/denial/version-skew tests and equivalent-authority report tests pass over real HTTP; inference keys cannot call control; MCP profile unchanged. |
| 3. Controller supervisor and constrained ACP profile | Reservations/idempotency, server preparation of installed agents/workspaces, owner/method/permission checks, bound route/cost, process cleanup, quotas/logs. Use in-process test managers. | Fake harness proves a single spawn, native identity, restrictions, cancellation, revocation, lease cleanup, and no secret/env leakage without needing a terminal. |
| 4. WebSocket and headless vertical slice | `ConnectTo` bridge, versioned framing and bounds, remote `spawn -p`/`acp prompt`, timeout/NDJSON integration. Initially no resume advertisement. | Two-process fake host/client completes prompts, permissions, cancellation and failure with expected output; socket loss is explicit/fail-closed in preview. |
| 5. Reliable reconnect | Retained logical connection, bidirectional ACK/replay, epoch fencing, all buffer/deadline failure modes, consumer lag handling. | Fault injection at every request/response/permission/ACK boundary causes no duplicate execution/output or silent loss within grace. Capability changes to `reconnect:true` only now. |
| 6. Interactive and discovery parity | Remote `chat`, piped chat, injected action ports, host/workspace UI, honest route/cost gating, server prompt commands, `agents list/check`, standalone `acp commands`. | Same renderer and action reports work on two computers; no agent or provider credential needed on A. Permission context and reconnect state are visible and accurate. |
| 7. Release acceptance and operational docs | Live pinned adapters, reverse-proxy recipe, diagnostics/metrics, cross-version acceptance, skill/development docs, final review of limits and defaults. | All acceptance gates below pass; remove preview qualification, keep service opt-in. |

Each PR updates this spec's implementation status only for demonstrated behavior;
maintain a small progress ledger if execution spans tasks. No dead scaffolding:
contract types must have current serialization/dispatch/test consumers in their
PR. Follow AGENTS.md (no new `allow`, panic shortcuts, or public re-exports).
When CLI/config/harness wiring changes, update `skills/bitrouter/` in the same
PR. Keep its top-level skill compact; add a remote reference for detail. Change
plugin manifests only if their actual MCP invocation changes. Published product
docs belong in `bitrouter-docs`; link the release work there without changing
its repository as an accidental part of implementation.

Enable initially for a host owner using one workspace and the maintained
Claude/Codex adapters. Observe launch failures, orphan processes, cancellation,
memory ceilings, and reconnect success before broader opt-in use. Rollback is
disable control, drain live controllers, revoke remote tokens if necessary, and
remove reverse-proxy exposure. Preserve workspace/harness history. Inference
operation and local CLI remain available throughout a control-only rollback.

## 12. Verification and release acceptance

### 12.1 Required automated tests and guards

- Table/schema guards: every remote action route resolves to an exposed row and
  matching typed input/output schema; every exposed row has a handler. New
  resource routes have explicit auth, scopes, body limits, and request/response
  contracts. Provoke guards with a deliberate extra/missing route in a fixture.
- Report parity: local CLI, remote CLI and TUI port output serialize/render
  equally for the same B runtime and equivalent authorization. Restricted status
  omits host spend; unpriced values and live/config provenance remain truthful.
- Context purity: with A's config missing, malformed, secret-filled, or pointing
  to an unrelated daemon, remote commands still use only B. Unsupported commands
  perform no local writes or daemon auto-start. Context/endpoint precedence and
  bad flag combinations fail before allocation.
- Auth matrix: absent/wrong/expired/revoked tokens, inference-key substitution,
  insufficient scopes, owner mismatch, attach replay, cross-origin redirect,
  malformed origin, concurrent writer, stale epoch, ticket expiry, and quota
  exhaustion. Verify no unauthorized agent process starts.
- Host constraints: traversal/symlink roots, arbitrary commands/env/cwd/MCP
  definitions, uninstalled adapters, forged native IDs, history methods,
  endpoint/auth/sandbox config changes, client tooling callbacks, and unknown
  extension calls fail at the server even with a modified client.
- Lifecycle faults: failed spawn/init/auth, dropped create response, duplicate
  idempotency key, owner delete during startup, stalled output, permission wait,
  child crash, process-group shutdown, token revocation during a turn, daemon
  restart, resource cleanup race, and unavailable inference proxy.
- Reliability model tests: drop each direction before/after dispatch and ACK,
  replay duplicate messages, interleave permission responses and cancel, fence
  half-open old sockets, overflow each bound, and exceed grace. Assert original
  ACP initialize count and fake tool execution count are one. Assert no output
  duplicate, silent lag, approval from silence, or new prompt retry.
- Route/cost tests: multiple controllers/principals, shared skip-auth honesty,
  request precedence, lease retention during detach and removal on terminal
  cleanup, descendant attribution, absent/unpriced cost, and no synthesized usage.
- UI tests: remote location shown in permission/file cards; unavailable commands
  explain scope/capability; detached input stays a draft; cancel uncertainty is
  visible; credentials and control sequences never reach the terminal/logs.
- Dependency/profile guards: renderer remains app/async/network-free; SDK does
  not depend on app/MCP runtime; existing CLI/TUI guards, MCP HTTP exact tool
  profile, and `multitenant_http` tests still pass.

For source PRs run the AGENTS.md gates: `cargo nextest run --all-features` (or
`cargo test --all-features`), `cargo clippy --all-features`, and `cargo fmt --
--check`. Also run focused network/transport fault tests and existing feature
isolation/public API checks when their dependency surfaces change. This
documentation-only proposal requires markdown/link/diff checks, not Rust builds.

### 12.2 Live two-computer acceptance

Use a disposable approved repository on B and separate client A. Keep A free of
ACP adapters/provider credentials and deliberately give it different local
models so accidental fallback is detectable. Use exact maintained adapter pins.

1. Configure an authenticated TLS reverse proxy on B; default-disabled control
   is unreachable until explicitly enabled. Validate certificate failure and
   inference-key rejection before running an agent.
2. Run remote status/models/preview and compare to B's local reports under
   equivalent authority. Read-only credentials cannot allocate an agent or see
   usage without its additional scope.
3. Run a headless task with a harmless file read and an attempted write. Verify
   deny-all/approve-reads, exit semantics, and clean NDJSON/text/quiet streams.
   Confirm process tree, workspace access, and model traffic occur on B.
4. In remote chat, inspect commands/status/models/preview; set/reset a route;
   observe true metered provenance when the harness emits usage. Deny and approve
   individual harmless fixture edits, verifying only B changes.
5. Interrupt networking during streaming, a pending permission, and a route
   response. Restore it within grace; keep one agent, native session, prompt,
   tool execution, decision, and visible output. Route/footer agrees with B.
6. Exceed grace and replay limits; see an explicit error and bounded cleanup.
   Cancel with only HTTPS available, then with both channels unavailable; verify
   confirmed versus unconfirmed wording and final server cleanup.
7. Revoke a credential and restart the server mid-turn. Verify denied reconnect,
   lost-controller explanation, no automatic task resubmission, no stale lease,
   and no surviving owned child processes.
8. Test missing agent authentication; diagnostics direct the operator to B.
   Test a second owner/credential cannot attach to, delete, or read logs from
   the first one's controller. Exercise size/rate limits and observe bounded
   memory and audit outcomes.
9. Disable control and run existing local CLI/TUI/MCP/inference smoke tests.
   No remote tokens appear in model requests, child env, ACP metadata, stdout,
   captured traces, or exported diagnostics.

Acceptance evidence records host/client versions, adapter pins, enabled
capabilities, sanitized transcripts/event counts, actual process cleanup, and
known harness limitations. Live model runs require the operator's normal test
credentials and a bounded fixture; a protocol-only fake test is not evidence of
Claude/Codex endpoint or identity conformance.

## 13. Review decisions and rejected alternatives

The defaults below are the proposed implementation baseline, not blocking
questions to the implementer. Change them at spec review if product priorities
differ; otherwise implement them as written.

| Review decision | Recommended answer | Consequence of choosing differently |
| --- | --- | --- |
| Canonical UI command names? | Pending the parallel UI review: prefer `run`/`tui` with approved compatibility aliases; examples here remain current `spawn -p`/`chat`. | Only dispatch/help/migration changes; do not make transport or controller APIs depend on command spelling. |
| Trusted host or hostile tenancy? | Trusted host/operator for v1. | Hostile tenancy requires isolated OS execution, harness history/auth and inference principals before sessions ship. |
| TLS/listener deployment? | Dedicated loopback control listener behind TLS proxy. | Native TLS or public HTTP binds require a separately tested certificate/listener configuration and rollout. |
| Standard draft transport or custom stable-v1 wrapper? | Explicit BitRouter WS transport; stable ACP v1 inside. | A standard transport needs pinned upstream implementation/conformance; draft support alone does not provide replay guarantees. |
| Reconnect scope? | Same client process/controller, 60-second grace; no background work. | Restart attachment/durable jobs require persistent request identity, authorization/history ownership, and recovery semantics. |
| Native history and multiple sessions? | One new native session per remote controller. | Browsing/loading shared harness history needs durable ownership/isolation and capability filtering first. |
| Inference principal per control credential? | Operator-selected normal inference credential/profile; document shared local identity when skip-auth. | Require separate normal keys and scoped metering if independently billed principals are needed. |
| Detailed usage and request history in first milestone? | Defer detailed history; scoped host spend in status is enough. | Moving the app-only reports/queries adds a separate reviewable action migration. |
| Remote `acp serve` bridge? | Defer until CLI/TUI release. | A generic manager needs a clearly advertised constrained profile and callback/capability conformance. |

Rejected alternatives:

- **Overload `--base-url`.** It selects model inference today; using it to also
  select agent placement would silently change where files and commands execute.
- **Expose or tunnel raw daemon IPC as the product protocol.** It grants host
  administrative authority, trusts declared principals/env, and lacks negotiated
  HTTP schemas/scopes. An operator's SSH tunnel to the authenticated control
  listener is fine; raw IPC does not become an unauthenticated remote API.
- **Force CLI/TUI through MCP.** Existing MCP profiles are intentionally narrower,
  and ACP already provides the bidirectional session/permission semantics. Reuse
  action ports, not an MCP transport detour or a widened cloud tool profile.
- **Run agents on A with remote inference.** That is the current explicit
  `--base-url` topology and fails the required workspace/process placement.
- **Build independent HTTP prompt streaming before ACP networking.** It duplicates
  cancellation, permissions, route/cost and lifecycle logic; one remote ACP
  connection can serve both presentation modes.
- **Replay only notifications, then retry the prompt.** A lost response does not
  mean the prompt was not executed. Bidirectional sequence/deduplication and a
  retained ACP connection are needed for safe in-process reconnect.
- **Claim a workspace allowlist or new token attests the harness.** Neither
  constrains an executing process's OS access nor proves native correlation
  headers. State the trusted-host boundary and use normal inference principals.
- **Add remote start/restart as ordinary client commands.** A stopped server
  cannot receive HTTP, and restart/credential/environment management belongs to
  host supervision. Session termination is a narrower owned-resource operation.
