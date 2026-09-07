# Spec: unified agent interfaces — native shortcuts, Code TUI, headless run, and one ACP bridge

Status: **proposed for review** · Date: 2026-09-07

Baseline: `45ac00bd` (`codex/remote-control-tui`)

This document defines the next public interface for launching native coding
agents, controlling ACP-compatible agents from a BitRouter UI or script, and
exposing BitRouter through MCP. It incorporates the maintainer discussion that
followed the remote-control MVP.

The product has two primary ways to control an ACP agent:

- `bitrouter code` for a person; and
- `bitrouter run` for automation or another agent.

It has one current local raw ACP transport entry point:

- `bitrouter acp serve <agent>` for an ACP client that launches an agent
  command and communicates over stdio.

`acp serve` is a BitRouter CLI name, not an ACP protocol requirement. A future
network-capable ACP client may instead connect directly to an ACP endpoint
exposed by the BitRouter daemon. Both transports must terminate in the same
ACP service and controller/session implementation.

Native harness shortcuts such as `bitrouter claude` and `bitrouter codex` are
launchers, not additional BitRouter agent UIs. They hand the terminal to the
harness's own interface after applying BitRouter's routing integration.

The first implementation does **not** make BitRouter the durable owner of ACP
sessions, run a persistent ACP supervisor in the daemon, or add remote ACP.
Harness-native session IDs and storage remain authoritative.

---

## 1. Decision summary

The target public surface is:

```text
Native harness UI
  bitrouter claude [options] [-- <claude args>]
  bitrouter claude-code [options] [-- <claude args>]  # alias
  bitrouter codex [options] [-- <codex args>]
  bitrouter launch <agent> [options] [-- <agent args>]

BitRouter-owned ACP clients
  bitrouter code [agent] [session options]
  bitrouter run <agent> [prompt | -] [session/output options]

Protocol plumbing (normally launched by a client or plugin)
  bitrouter acp serve <agent> [controller options]  # local stdio
  bitrouter mcp serve                               # local stdio

MCP diagnostics
  bitrouter mcp check [server]

Agent management
  bitrouter agents list
  bitrouter agents inspect <agent>
  bitrouter agents check [agent]
  bitrouter agents conformance <agent>
  bitrouter agents scaffold <agent>
```

The visible `spawn` command is retired. `acp prompt` and `chat` are also removed
from public help because they duplicate `run` and `code`. Compatibility aliases
remain temporarily, sharing the canonical implementation.

The following distinctions are intentional:

| Interface | Owner of presentation | Unit of work | Lifetime |
| --- | --- | --- | --- |
| `claude`, `codex`, `launch` | Native harness | Native harness invocation | Child process |
| `code` | BitRouter | Interactive ACP session | `code` process in the first release |
| `run` | BitRouter | One ACP turn, optionally against a native saved session | Command process |
| `acp serve` | External ACP client using stdio | One controller connection carrying one or more native sessions | Spawned-command connection |
| `mcp serve` | MCP client/host | Bounded BitRouter actions and resources | MCP connection |

These commands share agent resolution, route planning, capability negotiation,
session control, permission evaluation, and typed action reports. They do not
share presentation semantics merely to make the command tree look symmetric.

---

## 2. Relationship to existing specs

This document changes public naming and product scope while preserving the
controller invariants in [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md).

It supersedes:

- [`SPAWN_SPEC.md`](SPAWN_SPEC.md) where that document makes `spawn` the
  visible ACP umbrella;
- [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) where it selects `chat` as the
  BitRouter-owned interactive command and requires an inline-only renderer;
- the command naming in [`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md)
  that uses `tui <agent>` or `chat <agent>`; and
- the statement in `ACP_CONTROLLER_SPEC.md` §22.5 that a session lifecycle UI
  is permanently outside the product roadmap.

It retains these existing decisions:

- one controller owns one harness process and one ACP connection;
- generic ACP clients may create several harness-native sessions on that
  connection;
- BitRouter never replaces a harness-native ACP session ID;
- BitRouter does not persist a shadow transcript or session catalog;
- endpoint configuration and BitRouter route selection remain distinct;
- route control and router-attributed cost remain capability-probed extensions;
- native harness configuration is per-process or temporary by default; and
- remote ACP remains deferred while the upstream network transport is draft.

Showing several harness-native saved sessions in Code does not imply live
same-connection product multiplexing. The first implementation still exposes
one active Conversation and one process-owned controller; session list/load/
resume merely selects which harness-native session that controller operates.

This document reopens `ACP_CONTROLLER_SPEC.md` §22.7 only as a future trigger:
a daemon-owned supervisor becomes valid if BitRouter deliberately adds
background work or multi-client attachment. It is not part of the phases in
this spec.

---

## 3. Goals and non-goals

### 3.1 Goals

1. Give users one memorable BitRouter TUI, `bitrouter code`, from which they
   can select and control any locally available ACP-compatible agent.
2. Give automation one canonical headless command, `bitrouter run`.
3. Give command-spawning ACP clients one canonical local stdio entry point,
   `bitrouter acp serve`.
4. Make common native harness launches as direct as `bitrouter claude` and
   `bitrouter codex` without losing the generic `launch` escape hatch.
5. Resolve a human-facing agent name consistently across native, TUI,
   headless, diagnostic, and protocol entry points.
6. Expose harness-native list/load/resume/close lifecycle operations in the
   Code TUI when the harness advertises them.
7. Let one ACP agent delegate a bounded task to another by invoking the same
   `bitrouter run` contract a human or script uses.
8. Keep CLI stdout stable and machine-parseable, TUI behavior capability-led,
   ACP protocol-transparent, and MCP action-oriented.
9. Make local and remote operational actions share one neutral action contract.
10. Preserve current routing, authentication, permission, metering, and
    terminal-restoration safety.

### 3.2 Non-goals

- A BitRouter-owned transcript database or replacement session identifier.
- Keeping an ACP controller alive after `code`, `run`, or `acp serve` exits.
- Reattaching a second client to an already-running controller.
- Treating every unknown top-level subcommand as a dynamic agent name.
- Silently modifying a native harness's permanent configuration.
- Installing software without an interactive user's approval.
- Turning MCP into an interactive ACP transport.
- Exposing raw agent execution over the HTTP-only remote-control MVP.
- Implementing remote ACP before its transport and security model are stable.
- Guaranteeing stable behavior for draft ACP fork, delete, provider, usage, or
  network-transport proposals.

---

## 4. Product vocabulary

| Term | Meaning |
| --- | --- |
| agent | The user-facing coding agent, such as Claude, Codex, OpenCode, or a configured custom agent |
| harness | The native executable and UX owned by that agent project |
| adapter | The ACP-speaking process BitRouter launches to control a harness |
| facet | A way an agent can be driven: native UI or ACP |
| controller | BitRouter's connection-level ACP proxy around one adapter process |
| native session | An ACP session created and identified by the adapter/harness |
| Code TUI | BitRouter's full-screen ACP client, entered with `bitrouter code` |
| action | A bounded typed operation such as status, list models, route preview, or recent requests |
| available agent | A catalog or configured agent BitRouter can start on demand; not necessarily a running process |
| connected agent | A live adapter/controller connection owned by the current process |

Product copy must not call every configured agent “connected.” Until a
supervisor exists, most agents are available and launched on demand.

---

## 5. One agent resolver, two facets

The catalog must expose one product identity with optional native and ACP
facets. A conceptual shape is:

```rust
struct AgentDefinition {
    id: AgentId,
    aliases: Vec<AgentAlias>,
    native: Option<NativeFacet>,
    acp: Option<AcpFacet>,
}
```

This is a design shape, not a requirement to add heap-backed vectors or this
exact Rust type. Registry generation may continue to use static data.

Resolution order is:

1. an exact configured `agents:` key;
2. an exact canonical catalog ID;
3. an unambiguous catalog alias; then
4. an error naming the closest available agents and the required facet.

An exact configured name always wins over a built-in alias so user-owned
configuration remains addressable. Ambiguous aliases are rejected; BitRouter
never guesses.

The same input may resolve to different facets by command:

```text
bitrouter claude          -> Claude native facet
bitrouter code claude     -> Claude ACP facet
bitrouter run claude ...  -> Claude ACP facet
```

Machine output always carries the resolved canonical agent and adapter IDs.
Human output may keep the friendly name.

### 5.1 Initial native shortcuts

The initial first-class shortcuts are:

- `bitrouter claude`, canonical;
- `bitrouter claude-code`, an alias of `claude`; and
- `bitrouter codex`, canonical.

Additional shortcuts require an explicit CLI change and compatibility review.
They are not generated automatically from registry data. All other native
facets remain reachable through `bitrouter launch <agent>`.

This avoids making a registry update unexpectedly reserve a top-level command
or turning a typo such as `bitrouter stats` into process execution.

---

## 6. Native harness shortcuts and `launch`

### 6.1 Command grammar

```text
bitrouter claude [--model ID] [--base-url URL] [--no-install]
                 [--no-start] [--check] [-- <claude args...>]

bitrouter codex  [--model ID] [--base-url URL] [--no-install]
                 [--no-start] [--check] [-- <codex args...>]

bitrouter launch <agent> [--model ID] [--base-url URL] [--no-install]
                         [--no-start] [--check] [-- <agent args...>]
```

`launch --agent <id>` remains a hidden compatibility spelling; the canonical
generic form uses a positional agent.

### 6.2 “Set up with BitRouter”

Launching a native harness performs the current reversible setup:

1. resolve the native facet;
2. verify the executable and the requested model/route;
3. derive or accept the BitRouter inference endpoint;
4. auto-start a local daemon unless `--no-start` is set;
5. resolve the inbound BitRouter credential;
6. render the harness's environment, argument, or temporary-config overlay;
7. inject supported BitRouter MCP gateways; and
8. start the native child and propagate its exit status.

It does not edit the harness's normal user configuration. A harness that needs
generated config receives it in BitRouter's launch area and only for that
invocation.

If a supported executable is missing:

- at an interactive terminal, BitRouter may offer the reviewed official
  installer and must receive explicit consent;
- without a terminal or with `--no-install`, it fails with the install command;
- it never interprets an install prompt as permission to modify unrelated
  harness configuration.

Native shortcuts are local process commands. A remote `--context` is rejected
before daemon start, installation, or child-process side effects. An explicit
`--base-url` may point the local harness's inference traffic at a remote model
endpoint, but it does not make the native UI remote.

---

## 7. `bitrouter code`: the BitRouter-owned ACP TUI

### 7.1 Command grammar

```text
bitrouter code
bitrouter code <agent>
bitrouter code <agent> --load <native-session-id>
bitrouter code <agent> --resume <native-session-id>
bitrouter --context <name> code
```

`--load` and `--resume` are mutually exclusive and capability-gated.
`code <agent>` opens a new native session by default. Bare `code` opens the
home screen.

`bitrouter tui` and `bitrouter tui <agent>` remain hidden compatibility aliases
for `code` and `code <agent>` respectively.

Under the HTTP-only remote-control MVP, `--context <remote> code` opens only
the operations views. `--context <remote> code <agent>` fails before local
config loading, daemon start, or agent launch. Remote agent sessions remain
deferred.

### 7.2 Information architecture

The Code TUI is one full-screen application with these primary destinations:

| View | Responsibility |
| --- | --- |
| Home | Connection/target identity, quick actions, recent errors |
| Agents | Available agents, native/ACP facets, installation and readiness |
| Sessions | Native sessions reported by the selected ACP adapter |
| Conversation | Messages, thoughts, tool calls, permissions, composer, session controls |
| Routes | BitRouter route preview and the selected session route lease |
| Requests | Settled inference requests and attributed cost evidence |
| Models | Routable models and provider fallback chains |
| Integrations | Configured MCP servers, reachability, and advertised tools |

The first delivery may place Routes, Requests, and Models under one Operations
destination, but it must not build a second TUI application to do so. The
Integrations view is an inspector for configured servers; a public-registry
marketplace is not an acceptance criterion.

### 7.3 Terminal model

`code` uses the alternate screen. A multi-view application with session
selection, operation tables, tool cards, and future agent tabs cannot maintain
coherent navigation while also treating the shell's scrollback as its document
store.

Replacing the current inline ACP renderer creates obligations:

- internal transcript scrolling and tail-follow;
- search within the current rendered transcript;
- selectable/copyable tool output where the terminal supports it;
- an explicit transcript export action;
- no loss of a draft when switching operations views;
- a final concise session summary or export path on exit when requested; and
- unconditional alternate-screen, cursor, paste-mode, and raw-mode restoration
  on normal exit, error, signal, or panic.

The TUI must not silently persist transcripts merely to implement scrolling.
Its in-memory journal is presentation state. Explicit export is user-owned
output, not a second session authority.

### 7.4 Session lifecycle

The TUI presents only lifecycle operations the initialized adapter advertises:

- new;
- list;
- load with history replay;
- resume without replay;
- close active resources;
- fork, clearly marked experimental while upstream remains draft; and
- delete, clearly marked destructive and experimental while upstream remains
  draft.

`load` and `resume` are different user actions and must not be represented by
one ambiguous “continue” button. Help text explains whether history will be
replayed.

Close and delete are never synonyms:

- close releases an active session's resources and need not remove history;
- delete asks the harness to remove the session from its history semantics.

Delete requires an explicit confirmation naming the agent and native session.
BitRouter does not promise whether an adapter implements soft or hard delete.

If list is unavailable, the Sessions view says so and still offers New. It does
not scan Claude, Codex, or other private storage directly.

### 7.5 Process and persistence semantics

In the first release:

1. the Code process launches and owns the controller and adapter child;
2. the controller may carry the native sessions the user opens through it;
3. switching to an operations view does not stop the active controller;
4. exiting Code disconnects and terminates the adapter child;
5. disconnect does not automatically close or delete native sessions; and
6. a later Code process rediscovers durable sessions through the adapter's ACP
   lifecycle methods.

BitRouter persists neither transcripts nor a shadow list. At most, future UI
preferences may remember a last selected agent or view; they cannot claim a
session exists when the adapter does not report it.

The first implementation supports one active Conversation at a time. Multiple
independent live controller tabs may be added later inside the Code process,
but are not an acceptance criterion and must not be implemented by sharing one
controller across different harnesses.

### 7.6 Agent-native config versus BitRouter route

The Conversation UI must show two separate controls when available:

- **Agent configuration:** ACP session config options such as model, mode, or
  reasoning effort; and
- **BitRouter route:** the session route lease used by router policy.

Changing an agent model config option sends the standard capability-gated ACP
operation. Changing a BitRouter route uses the capability-probed
`_bitrouter/route/*` extension. Provider endpoint configuration remains an
internal controller-to-adapter operation and is not shown as either selector.

The footer names provenance explicitly, for example:

```text
agent model: sonnet · route: bitrouter/auto · cost: router USD 0.042
```

Missing capabilities produce an unavailable explanation, not a dead control.

### 7.7 Permissions

Interactive permissions remain modal and default to asking the user. Escape,
cancel, or teardown never implies approval.

The shared policy evaluator may pre-decide a request when the user supplied a
policy. Unmatched interactive requests are prompted; unmatched headless
requests are denied. Any UI or help text for automatic read approval must say
“agent-declared read,” because the ACP tool kind is a claim from the adapter,
not an OS sandbox or independent filesystem proof.

---

## 8. `bitrouter run`: the headless ACP client

### 8.1 Command grammar

```text
bitrouter run <agent> [<prompt> | -]
  [--prompt-file PATH]
  [--load NATIVE_SESSION_ID | --resume NATIVE_SESSION_ID]
  [--cwd PATH]
  [--format ndjson|text|quiet]
  [--result-schema JSON|@PATH]
  [--turn-timeout SECS]
  [--approve-all|--approve-reads|--deny-all]
  [--permission-policy JSON|@PATH]
  [--direct] [--model ID] [--base-url URL] [--no-start]
  [-c PATH]
```

Prompt precedence is explicit and exclusive:

1. `--prompt-file PATH`;
2. positional prompt;
3. `-`, meaning stdin; or
4. implicit stdin only when stdin is not a terminal.

No prompt at an interactive terminal is an error with examples. BitRouter does
not open the Code TUI implicitly from `run`.

New session is the default. `--load` and `--resume` are capability-gated and
operate on harness-native IDs. This lets automation continue native work
without introducing a separate `sessions` command hierarchy.

### 8.2 Output

The canonical streaming format name is `ndjson`. `json` remains a hidden value
alias during migration because the old `--format json` already means NDJSON.

Every NDJSON line uses a BitRouter-owned event envelope:

```json
{"version":1,"seq":0,"type":"session","agent":"claude","adapter":"claude-acp","session_id":"native-id","via":"http://127.0.0.1:4356"}
{"version":1,"seq":1,"type":"message_chunk","text":"..."}
{"version":1,"seq":2,"type":"result","stop_reason":"end_turn"}
```

Requirements:

- `version` identifies the BitRouter event contract, not the ACP wire version;
- `seq` is monotonically increasing within this process stream;
- the first success event is `session`;
- exactly one terminal `result` or `error` event is emitted;
- progress, routing notices, and logs go to stderr;
- terminal control sequences never enter stdout;
- `text` renders a readable transcript;
- `quiet` emits only final assistant text; and
- result-schema fields remain on the terminal result event.

Raw ACP payload evolution must not silently redefine this CLI contract. The
translator maps protocol messages into versioned BitRouter events.

Global report flags `--json` and `--human` are not shown for `run`. During
migration, a global `--json` may select NDJSON and `--human` may select text,
but the command-specific `--format` is the canonical spelling.

### 8.3 `--no-wait` retirement

`--no-wait` is removed from public help. The current implementation reports a
submission and immediately tears down the controller and child, so it does not
provide the background-work behavior the name implies.

The flag remains a hidden compatibility option for one release and prints a
warning to stderr. It must not be renamed `--detach` until a supervisor owns a
continuing controller and returns an attachable controller or job ID.

### 8.4 Exit behavior

Exit categories are centralized rather than selected independently by each
alias. At minimum they distinguish:

- successful terminal result;
- CLI usage/configuration error;
- agent/adapter not found or not ready;
- routing/authentication/transport failure;
- turn timeout or cancellation;
- permission refusal; and
- result-schema failure after the bounded repair attempt.

The existing permission-refusal exit behavior remains compatible. Exact
numeric assignments are frozen with implementation tests before release.

---

## 9. ACP transport surface

### 9.1 Command grammar

```text
bitrouter acp serve <agent>
  [--turn-timeout SECS]
  [--direct] [--model ID] [--base-url URL] [--no-start]
  [-c PATH]
```

`--agent <id>` remains a hidden compatibility spelling.

The command description is:

> Run a BitRouter ACP controller for one agent adapter over stdio. One client
> connection may carry multiple harness-native sessions.

It must not claim that the process represents exactly one session.

The word `serve` describes this process acting as the ACP agent/server side of
the stdio connection. ACP does not standardize this command name. Existing
command-oriented clients may be configured to spawn any executable and
arguments that speak ACP on stdin/stdout; for BitRouter, those arguments are
`acp serve <agent>`.

### 9.2 Protocol purity

- stdin/stdout contain ACP JSON-RPC only;
- logs, migration warnings, and diagnostics go to stderr;
- global `--json`, `--human`, and TUI presentation flags are absent from help;
- client capabilities and `_meta` are forwarded during initialization;
- client-visible session IDs remain harness-native;
- client lifecycle requests are forwarded when advertised;
- unknown extension payloads remain transparent; and
- provider endpoint setup remains inaccessible to the client.

`turn_timeout` is an optional controller policy, never a hidden default that
overrides a client's own cancellation behavior.

### 9.3 Stable and experimental ACP tiers

The implementation and docs distinguish:

| Tier | Examples | Policy |
| --- | --- | --- |
| Stable ACP v1 | initialize, new, prompt, cancel, config options, list, load, resume, close | Supported when advertised |
| Adapter-private compatibility | draft provider list/set used by maintained pins | Controller-to-adapter only, exact pins, never client-facing |
| BitRouter extension | `_bitrouter/route/*`, cost provenance metadata | Versioned and capability-probed |
| Upstream experimental | fork, delete, usage, network transport while draft | Feature-gated, labeled experimental, no compatibility promise beyond the pin |

The controller may continue to forward an experimental method transparently,
but product copy and acceptance tests do not call it stable merely because the
pinned schema contains it.

### 9.4 One ACP service, multiple transports

`acp serve` is not the permanent network architecture. The logical ACP service
may eventually be exposed in three ways:

| Transport path | Intended client | Status |
| --- | --- | --- |
| `bitrouter acp serve <agent>` over stdin/stdout | Local client that launches an agent command | Current canonical path |
| Direct daemon ACP endpoint | Network-capable ACP client | Deferred until the remote transport is stable and supported |
| Local `bitrouter acp connect <context> <agent>` stdio bridge | Stdio-only client reaching a remote daemon | Deferred with remote ACP |

A network-capable client connects directly to the daemon endpoint and does not
run `acp serve` on its own computer. A stdio-only client may eventually spawn
`acp connect`, which preserves protocol-pure stdin/stdout while forwarding to
the selected remote ACP endpoint.

The direct endpoint must implement the complete bidirectional ACP transport,
including streamed updates and agent-to-client requests. It is not a set of
ordinary REST control actions and is not the existing read-only remote-control
API. Authentication, workspace authorization, agent selection, connection
ownership, cancellation, and teardown are server responsibilities.

BitRouter must select the downstream agent outside the standard session
lifecycle—for example through deployment configuration, an authenticated
mapping, or an endpoint path chosen by the eventual transport design. It must
not require a proprietary field in `session/new` merely to choose an agent,
because that would make otherwise compatible clients BitRouter-specific.

All three paths terminate in the same controller/session kernel. Adding a
network transport does not create another agent lifecycle interface.

---

## 10. Agent-to-agent delegation

The first delegation mechanism is the headless CLI itself:

```sh
bitrouter run codex \
  --result-schema @review.schema.json \
  "Review this patch and return findings only"
```

An agent with an approved terminal execution capability can invoke it exactly
as a script would. `run` launches the target adapter on demand; it does not
require a resident pool of “connected” agents.

Delegation inherits the same requirements as direct headless use:

- deny permissions by default;
- bound the turn with a timeout;
- allow an explicit result schema;
- preserve the caller's working directory only when permitted;
- identify parent and child in tracing/metering without treating the
  identifiers as authentication; and
- cap concurrency through existing process/resource limits.

Before BitRouter advertises recursive delegation as a product feature, it must
add and test:

1. an allowed-target policy;
2. a maximum delegation depth;
3. a per-child timeout and cost/budget boundary;
4. parent/child trace correlation;
5. environment inheritance rules; and
6. behavior when the child itself attempts delegation.

A future MCP `delegate` tool may wrap the same typed run action for ACP agents
without shell access. It is deferred until the CLI action and safety policy are
stable. The MCP tool must not create a second session implementation.

---

## 11. Shared internal session architecture

The current `acp_cli.rs` composition root contains routing, process launch,
controller construction, one-shot presentation, terminal presentation,
permissions, lifecycle probing, and observability. The refactor separates
these responsibilities without changing the controller kernel.

Conceptually:

```text
AgentResolver
    -> AgentTarget { canonical id, requested facet, invocation }

SessionSpec
    -> agent target
    -> routing target/model
    -> cwd + MCP servers
    -> new/load/resume selection
    -> timeout + auth capability policy

SessionHost
    -> resolve routing
    -> launch adapter/controller
    -> negotiate capabilities
    -> return SessionHandle

SessionHandle
    -> AcpClient
    -> native session identity
    -> CapabilitySnapshot
    -> route/cost binding
    -> deterministic shutdown/reaping

Drivers
    -> OneShotDriver (`run`)
    -> CodeDriver (`code`)
    -> StdioAgentEndpoint (`acp serve`)

Presenters
    -> NdjsonPresenter
    -> TextPresenter
    -> QuietPresenter
    -> Code TUI state/reducer/view
```

The types need not use these exact names. The required dependency direction is:

```text
controller/session kernel <- drivers <- CLI composition
                           <- TUI state and rendering
```

The controller/session kernel has no dependency on clap, terminal rendering,
human report tables, or remote HTTP. The TUI crate receives plain state and
events and has no app/config/database/control-socket dependency.

### 11.1 Capability snapshot

After initialize, all drivers consume one normalized snapshot containing at
least:

- native session lifecycle support;
- ACP session config options;
- authentication methods and whether terminal auth is usable;
- MCP transports accepted by the adapter;
- advertised slash commands;
- BitRouter route-control methods;
- BitRouter usage/cost provenance support; and
- stable versus experimental classification known to this build.

Features are rendered or rejected from this snapshot. No driver infers support
from an agent name.

### 11.2 MCP gateway planning

The existing single gateway plan remains the source for native harness config
and ACP `session/new` descriptors. Before creating a session, the ACP driver
filters or adapts descriptors against the negotiated MCP transport capability.
Unsupported transports produce an explicit partial-integration report; they
are not sent optimistically and discovered through an opaque session failure.

Draft MCP-over-ACP transport is not required by this spec.

---

## 12. Shared operational actions

The actions contract must no longer be conceptually owned by the MCP adapter.
CLI, Code TUI, remote HTTP, and MCP are peers consuming neutral typed actions.

The neutral inventory includes at least:

| Canonical action ID | CLI | Code TUI | HTTP control | MCP |
| --- | --- | --- | --- | --- |
| `status` | `status` | Home/Operations | `GET /status` | `status` |
| `list_models` | `models` | Models | `GET /models` | `list_models` |
| `route_preview` | `route` | Routes/`/preview` | `POST /route/preview` | `route_preview` |
| `list_requests` | `requests` | Requests | `GET /requests` | scoped `list_requests`, when enabled |
| `skills_search` | `skills list` | not required | not exposed | `skills_search` |
| `skills_get` | optional future leaf | not required | not exposed | `skills_get` |
| `session_commands` | `agents inspect` | Conversation help | not exposed | not exposed |
| `session_route_set` | no standalone leaf | Conversation route picker | future session transport only | not exposed |
| `session_route_reset` | no standalone leaf | Conversation route picker | future session transport only | not exposed |

Each inventory row carries independently:

- canonical ID and version;
- input and report schema;
- read/write/destructive/idempotent effect;
- subject: portable, selected deployment, local host, or live session;
- required capability;
- permitted exposures;
- authorization scope; and
- inverse operation where one exists.

“Subject” and “transport exposure” are separate. A host-bound action can be
answered remotely by the selected host; host-bound must not mean local-only.

HTTP capabilities use versioned descriptors rather than an unversioned string
array. CLI spelling, slash-command spelling, HTTP path, and MCP tool name may
differ, but they all map to the same canonical action ID.

### 12.1 Requests as a first-class action

`status --requests` currently changes the report type instead of modifying a
status report. The canonical command becomes:

```sh
bitrouter requests [--limit N]
bitrouter --context workstation requests [--limit N]
```

`status --requests` remains a hidden compatibility spelling. The Code TUI and
HTTP endpoint use the same `list_requests` report.

An MCP `list_requests` tool is optional and profile-scoped because request
history is more sensitive than a model catalog. It is enabled for an
owner-trusted local stdio profile or the daemon endpoint with an explicit
operator scope, never merely because the backend can reach a database.

---

## 13. MCP interface

MCP remains BitRouter's bounded control, introspection, skills, and tool-gateway
surface. It does not become another interactive ACP client and does not run
model inference through a reduced second API.

MCP has two directions in BitRouter and product copy must name which one it
means:

- **origin server:** an MCP host calls BitRouter-owned actions and reads
  BitRouter-owned resources; and
- **upstream client/gateway:** BitRouter connects to configured third-party MCP
  servers and may expose their namespaced tools to a harness.

These roles share protocol code but are not one user workflow.

### 13.1 Minimal CLI surface

Only two canonical MCP commands remain in the target CLI:

```text
bitrouter mcp serve
bitrouter mcp check [server]
```

`mcp serve` is advanced transport plumbing, not a human interaction surface.
It runs the BitRouter origin server over protocol-pure stdio because many MCP
hosts and the BitRouter plugin manifests launch a configured subprocess. It is
normally invoked by a host, plugin, or native launcher rather than typed by a
person. It exposes only the local capabilities appropriate to that process,
including workspace-local skills when configured.

`mcp check [server]` is the one headless diagnostic for configured upstream
MCP servers. Its report includes transport, reachability, latency, capability
negotiation, and advertised tool names. With no server it checks every
configured server. Human and JSON rendering follow the normal bounded-report
contract.

The target CLI does not include an MCP registry browser, package installer, or
separate list/status/discover hierarchy. Those commands are conveniences, not
requirements of the MCP wire protocol, and they make BitRouter's client and
server roles harder to understand.

`mcp_servers:` remains the declarative source of truth. Code's Integrations
view may inspect configured entries and help a user author one. Browsing the
preview public registry or building a marketplace requires a separate product
decision and is not part of this spec.

First-party native launchers and plugin manifests configure `mcp serve`
automatically. Documentation may show a copyable command/URL block for other
hosts; a generic host-specific `mcp install` command is not required.

### 13.2 Streamable HTTP belongs to the daemon

MCP already defines a standard Streamable HTTP transport. The daemon, not a
second manually launched `mcp serve --transport http` process, owns the remote
origin endpoint:

```text
MCP host ── authenticated Streamable HTTP ──> BitRouter daemon /mcp-control
```

The existing `/mcp` upstream-aggregation endpoint remains a different role:
it exposes namespaced tools from configured third-party servers. The origin
`/mcp-control` endpoint exposes only authorized BitRouter actions. Combining
the two namespaces later requires an explicit authorization and collision
policy; it is not implied by using the same protocol.

The origin endpoint follows the same action capability descriptors and scopes
as the HTTPS control API. It validates `Origin`, defaults to loopback exposure,
requires authentication for non-loopback access, and never exposes a tool
merely because the daemon can call its implementation internally.

A network-capable MCP host connects to `/mcp-control` directly and needs no
BitRouter CLI on its computer. Only a stdio-only host needs a future local
compatibility bridge:

```text
MCP host ──stdio──> bitrouter mcp connect <context>
                              └──Streamable HTTP──> /mcp-control
```

`mcp connect` is deferred until a real supported host requires it. If added,
it is a protocol-pure proxy of server-advertised capabilities: no fallback to
the client computer, no mixing in client-local skills, and no local config or
database substitution after a remote failure.

Remote MCP provides bounded control/introspection without remote ACP or remote
agent process execution.

### 13.3 Compatibility cleanup

The existing commands migrate as follows:

| Existing | Target behavior |
| --- | --- |
| `mcp serve` | Retained; stdio is its canonical role |
| `mcp serve --transport http` | Replaced by the daemon's `/mcp-control` endpoint |
| `mcp serve --backend cloud` | Replaced by direct remote MCP, or future `mcp connect` for stdio-only hosts |
| `mcp install` | Hidden compatibility helper; first-party setup is automatic and other hosts use documented config |
| `mcp list`, `mcp search`, `mcp add` | Hidden during the compatibility window; no core-CLI replacement |
| `tools list`, `tools status` | Consolidated into `mcp check [server]` |
| `tools discover` | Hidden during the compatibility window; inspect declarative config or Code Integrations |

Protocol subprocess aliases must preserve stdout purity. Commands without a
canonical replacement remain available only for the declared compatibility
window and are then removed; they are not moved into a deeper namespace merely
to preserve surface area.

### 13.4 Tool contracts

Every MCP action returns structured content conforming to its advertised output
schema and supplies accurate read-only, destructive, idempotent, and open-world
annotations. An annotation is descriptive metadata, not authorization.

Long-running ACP sessions are not represented as ordinary MCP tool calls. A
future bounded `delegate` action may use MCP task support when that contract is
stable, but it still delegates to the same run engine and policy.

---

## 14. Option scoping and help design

Global options currently appear on commands where they have no meaning. The
new help surface follows these rules:

- `--json`/`--human` appear only on bounded report commands;
- `run` exposes `--format` instead;
- `code` exposes no output-format flag;
- stdio `acp serve` and `mcp serve` expose no presentation-format flag;
- `--context` appears only where the selected command supports a remote
  target;
- native shortcuts and `launch` reject `--context`;
- agent-session `code`/`run` reject remote context until remote ACP exists;
- `--config` and `--socket` are absent from a remote target's valid examples;
  and
- every ignored option is either removed or rejected—never accepted silently.

Compatibility parsing before the subcommand may remain temporarily, but it
must not make irrelevant flags look supported in leaf help.

---

## 15. Compatibility and migration

### 15.1 Command mapping

| Existing command | New canonical command | Migration behavior |
| --- | --- | --- |
| `launch -a claude` | `claude` or `launch claude` | Old form hidden, same implementation |
| `launch -a codex` | `codex` or `launch codex` | Old form hidden, same implementation |
| `spawn <agent> -p TEXT` | `run <agent> TEXT` | Hidden alias, warning on stderr |
| `spawn <agent> --serve` | `acp serve <agent>` | Hidden alias; protocol stdout unchanged |
| `spawn <agent> --check` | `agents check <agent>` | Hidden alias |
| `acp prompt --agent <agent> TEXT` | `run <agent> TEXT` | Hidden alias |
| `chat <agent>` | `code <agent>` | Hidden alias |
| `tui` | `code` | Hidden alias |
| `tui <agent>` | `code <agent>` | Hidden alias |
| `acp serve --agent <agent>` | `acp serve <agent>` | Hidden flag alias |
| `agents install <agent>` | `agents scaffold <agent>` | Hidden alias; still prints configuration only |
| `status --requests` | `requests` | Hidden flag alias |

All aliases call the same functions as their canonical replacements. Tests
compare exit status, stdout, and relevant stderr after removing the one allowed
migration notice.

### 15.2 Visibility and removal

The migration sequence is:

1. canonical forms land and old forms disappear from normal help;
2. old forms emit a concise stderr notice when a human invokes them;
3. protocol-server aliases preserve stdout purity and may suppress notices
   when configured as child processes;
4. release notes and the BitRouter skill use canonical forms only; and
5. aliases are removed at the next declared compatibility boundary, not by an
   undocumented patch release.

The repository skill and plugin manifests must change in the same
implementation commit as any affected CLI or MCP invocation.

---

## 16. Security and failure behavior

1. A native shortcut cannot install without interactive consent.
2. A remote context cannot accidentally launch or mutate local state.
3. Headless permissions default to deny.
4. “Approve reads” is based on agent-declared metadata and is documented as
   such.
5. Session route headers remain routing/correlation claims within the API
   principal, not proof of controller identity.
6. Provider/API credentials never appear in agent info, ACP `_meta`, NDJSON,
   TUI state dumps, reports, or logs.
7. A failed routing preflight happens before an adapter process starts.
8. Every launched process group is reaped on normal and abnormal teardown.
9. Cancelling with a permission request open denies the request.
10. Code TUI exit restores the terminal even when an adapter or renderer
    fails.
11. Delete is capability-gated, explicit, and confirmed; close is not called
    delete.
12. Recursive delegation is not enabled as a convenience without depth,
    target, concurrency, and budget controls.

---

## 17. Delivery phases

### Phase 0 — public naming and compatibility

- Add `code` as the canonical TUI command.
- Make `launch <agent>` and `acp serve <agent>` positional.
- Add `claude`, `claude-code`, and `codex` native shortcuts.
- Hide `spawn`, `chat`, `tui`, and `acp prompt` from normal help.
- Move preflight to `agents check [agent]`.
- Correct `acp serve` help from one session to one stdio client connection.
- Remove irrelevant global flags from leaf help.
- Update CLI docs, skill references, completions, and plugin manifests where
  their invocation changes.

Completion criterion: `bitrouter --help` communicates the target mental model
without requiring the reader to understand ACP.

### Phase 1 — agent resolver and shared session host

- Introduce facet-aware agent resolution and friendly aliases.
- Extract the shared session specification, host, handle, shutdown, and
  capability snapshot from `acp_cli.rs`.
- Keep the existing controller kernel and native session identity.
- Drive `run`, `code <agent>`, and `acp serve` from the extracted layer.
- Make gateway injection capability-aware.

Completion criterion: each public driver chooses presentation only after one
shared launch/initialize path has returned the same resolved agent and
capability data.

### Phase 2 — headless contract

- Add stdin and prompt-file input.
- Add `--load` and `--resume`.
- Rename the canonical format to NDJSON.
- Version and sequence the event envelope.
- Centralize exit categories.
- Retire visible `--no-wait`.
- Preserve result-schema and permission behavior.

Completion criterion: scripts can consume one documented, versioned stream and
can create or continue a harness-native session without an alternate command.

### Phase 3 — Code TUI shell

- Merge the operations dashboard and ACP session entry into one full-screen
  application.
- Add Home and Agents views.
- Preserve operations behavior for local and remote HTTP contexts.
- Add internal transcript scroll/search/copy/export.
- Keep drafts across operations navigation.
- Preserve terminal and log safety.

Completion criterion: a user can launch bare `bitrouter code`, select an ACP
agent, complete a turn, inspect routing/requests/models, and return without
leaving the application or corrupting the terminal.

### Phase 4 — native session lifecycle and configuration

- Add Sessions view.
- Support advertised new/list/load/resume/close.
- Clearly gate and label fork/delete experiments.
- Render standard ACP session configuration.
- Keep agent model configuration separate from BitRouter route leases.

Completion criterion: the TUI can reopen harness-native durable sessions
without BitRouter storing a transcript or scanning private harness files.

### Phase 5 — neutral actions and MCP cleanup

- Move the action contract to a neutral home.
- Add canonical IDs/versions/exposure/auth metadata.
- Promote recent requests to `bitrouter requests`.
- Make HTTP capabilities versioned action descriptors.
- Keep `mcp serve` as the protocol-pure local stdio origin endpoint used by
  hosts, launchers, and plugin manifests.
- Consolidate configured-upstream diagnostics into `mcp check [server]`.
- Hide the registry/install and `tools` compatibility commands without
  recreating their hierarchy under new names.
- Mount the authenticated Streamable HTTP origin endpoint at `/mcp-control`
  in the daemon and retire the standalone HTTP `mcp serve` mode.
- Add the Code Integrations inspector for configured MCP servers.

Completion criterion: CLI, Code TUI, HTTP control, and MCP obtain the same
eligible report from the same action contract; network-capable MCP hosts
connect directly to the daemon, stdio hosts can still launch the local origin
server, and neither path falls back to the wrong machine.

### Phase 6 — bounded delegation

- Prove agent-to-agent use through `bitrouter run` end to end.
- Add target/depth/concurrency/budget policy.
- Add parent-child tracing and metering correlation.
- Decide separately whether an MCP `delegate` wrapper is justified.

Completion criterion: a parent agent can delegate a bounded task and consume a
structured result without uncontrolled recursive process creation.

### Deferred trigger — supervisor and remote ACP transports

Do not begin a daemon ACP supervisor merely to “future-proof” the design. Reopen
it only when an accepted requirement needs at least one of:

- a controller continuing after its client exits;
- attach/detach from more than one local client;
- durable background jobs;
- shared live spectators or writers; or
- a stable remote ACP transport whose server must own process lifetime.

At that point `SessionHost` gains a supervised implementation. `code` and
`run` remain clients; the TUI never becomes the persistence layer.

When a stable remote transport and real client demand justify it, the daemon
may expose a direct authenticated ACP endpoint. Network-capable clients connect
to it directly. A separate local `acp connect` stdio bridge is justified only
for clients that cannot configure a network ACP endpoint. Neither path changes
ACP lifecycle semantics or introduces a second session store.

---

## 18. Verification strategy

### 18.1 CLI contract tests

- top-level help shows `code`, `run`, `acp`, native shortcuts, and generic
  `launch`, but not compatibility aliases;
- typos remain unknown-command errors and never resolve dynamically;
- friendly aliases resolve to the correct facet and canonical IDs;
- native shortcuts and generic launch build byte-equivalent child
  environments/arguments;
- irrelevant flags are absent or rejected;
- stdin, prompt-file, and positional prompt precedence is enforced;
- aliases preserve canonical stdout and exit status; and
- ACP/MCP stdout contains protocol frames only.

### 18.2 Fake-adapter contract tests

Use deterministic fake ACP agents covering:

- no lifecycle capability;
- list/load/resume/close;
- config options and updates;
- advertised commands arriving before and after session creation;
- permission requests during streaming;
- missing/partial MCP transport support;
- route extension absent/present/partial;
- stable and experimental methods;
- malformed or lagging update streams; and
- clean and unclean child teardown.

Tests assert that native IDs pass through unchanged and no shadow session is
written.

### 18.3 TUI tests

- reducer/state tests for navigation, drafts, lifecycle, permissions, search,
  route versus model selectors, and errors;
- snapshot or buffer tests at narrow/wide terminal sizes;
- PTY tests for alternate-screen restoration, resize, paste, cancellation,
  signals, and failed child launch;
- live Claude/Codex smoke tests for new session and one prompt; and
- capability-dependent tests that unavailable controls explain why.

### 18.4 Action and MCP tests

- every exposed action has one canonical ID and version;
- report JSON matches across applicable CLI, HTTP, TUI driver, and MCP paths;
- MCP output schemas match structured content;
- stdio `mcp serve` writes only MCP frames to stdout;
- `/mcp-control` registers only authorized server-side actions and enforces
  authentication plus `Origin` validation;
- `mcp check` covers reachability, latency, negotiation, and advertised tools
  for one or all configured upstream servers;
- remote MCP never exposes client-local skills;
- remote failures never fall back to local config/database;
- recent requests remain bounded and scope-checked; and
- compatibility command mappings stay byte-equivalent where promised.

### 18.5 Workspace gates

Every source phase runs:

```sh
cargo nextest run --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p dist-helper -- check
git diff --check
```

Documentation-only review edits require at least `git diff --check` and any
available Markdown/link checker.

---

## 19. Acceptance criteria

The design is implemented when all of the following are true:

1. `spawn`, `chat`, `tui`, and `acp prompt` are absent from normal help and
   remain tested compatibility aliases for the declared window.
2. `bitrouter claude` and `bitrouter codex` perform the same reversible native
   integration as generic `launch` without editing permanent harness config.
3. `bitrouter code` is the only documented BitRouter-owned TUI entry.
4. Bare Code can select an available ACP agent and run a conversation.
5. Code operations use the same reports as the headless CLI locally and over
   the existing remote HTTP context.
6. Code session controls reflect negotiated ACP capabilities and keep native
   model/config options distinct from BitRouter routing.
7. Code can list/load/resume/close native sessions where supported without
   persisting a BitRouter session catalog or transcript.
8. Exiting Code terminates its live adapter/controller and preserves only what
   the harness natively persists or the user explicitly exports.
9. `run` accepts practical prompt input, emits a versioned NDJSON contract,
   defaults to denying permissions, and can load/resume when advertised.
10. `--no-wait` no longer claims background behavior.
11. `acp serve` is the documented local stdio ACP entry point and accurately
    describes its connection-level, multi-session protocol semantics.
12. Native, headless, TUI, and protocol commands resolve agent identity through
    one facet-aware resolver.
13. MCP remains a bounded action/resource interface: stdio hosts launch
    `mcp serve`, network hosts connect directly to the daemon's authenticated
    `/mcp-control` endpoint, and neither path adds remote ACP.
14. Delegation through `run` is bounded before being promoted as an agent-facing
    feature.
15. Remote ACP and persistent controller supervision remain absent until their
    explicit triggers are met.

---

## 20. Deliberately rejected alternatives

### 20.1 Every unknown command is an agent

Rejected because registry changes would mutate the top-level CLI namespace,
typos could execute processes, and future built-in commands could collide with
user configuration. Curated shortcuts plus `launch <agent>` are explicit.

### 20.2 `code` persists its own transcripts

Rejected because the harness already owns native IDs, replay, history, and
deletion semantics. A second store creates conflicting authorities and cannot
make unsupported adapters durable.

### 20.3 The daemon supervises every ACP agent immediately

Rejected for the initial implementation because neither one-shot run nor an
interactive TUI requires background lifetime or multi-client attachment. The
abstraction must permit a later supervisor without paying its operational and
security cost now.

### 20.4 `run --no-wait` means detach

Rejected because the current process owns the controller. Exiting it terminates
the child. Detach is only honest after a supervisor exists.

### 20.5 Merge ACP and MCP into one protocol

Rejected because ACP controls a bidirectional, stateful agent session while MCP
offers bounded tools and resources to a model or host. They share actions and
gateway planning, not wire semantics or lifecycle.

### 20.6 Native shortcuts rewrite user configuration

Rejected because per-process routing is reversible, testable, and does not
leave a harness broken when BitRouter is unavailable.

### 20.7 Keep both inline and full-screen BitRouter session TUIs

Rejected because it retains two navigation, rendering, terminal, and lifecycle
models under one product. `code` adopts the full-screen application obligation
and replaces shell scrollback with explicit in-app navigation and export.

### 20.8 Make MCP a large human-facing CLI hierarchy

Rejected because stdio serving, daemon HTTP serving, upstream-server
diagnostics, public-registry discovery, and host configuration are different
jobs. Only stdio transport plumbing and one bounded diagnostic need CLI
commands. First-party setup is automatic, remote hosts use the daemon endpoint,
and richer inspection belongs in Code rather than in a protocol-shaped command
tree.

---

## 21. Review decisions

Approval of this spec approves these product decisions together:

1. `code` is the canonical BitRouter TUI name; `tui` becomes compatibility.
2. `run` remains the canonical headless name.
3. `acp serve` is the sole documented local stdio ACP entry point; a future
   direct daemon endpoint is another transport for the same logical service.
4. `spawn`, `acp prompt`, and `chat` leave normal help.
5. `claude` is canonical, `claude-code` is its alias, and `codex` is a
   first-class native shortcut.
6. Generic `launch <agent>` remains for every other native facet.
7. Code is full-screen and initially owns one live controller at a time.
8. Code controls but does not persist harness-native sessions.
9. Background controller supervision, direct remote ACP, and the optional
   `acp connect` bridge are deferred.
10. Agent-to-agent delegation starts through bounded `run`; an MCP wrapper is a
    later decision.
11. Operational action contracts move toward a neutral home and include
    recent requests.
12. `mcp serve` remains advanced local stdio plumbing; `mcp check` is the only
    canonical MCP diagnostic; registry/install command families are retired.
13. Network-capable MCP hosts use the daemon's authenticated Streamable HTTP
    `/mcp-control` endpoint directly. A local `mcp connect` bridge is deferred
    for stdio-only hosts and does not add remote ACP.

Implementation must not begin by silently choosing a different answer to one
of these items. A changed decision amends this section first.

---

## 22. Authoritative references

- [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) — controller topology,
  identity, route leases, and harness-owned sessions.
- [`ACP_SAFETY_INVARIANTS.md`](ACP_SAFETY_INVARIANTS.md) — teardown,
  permissions, and process safety.
- [`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) — current shared action/report work.
- [`REMOTE_CONTROL_MVP_SPEC.md`](REMOTE_CONTROL_MVP_SPEC.md) — implemented
  remote HTTP read actions.
- [`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md) — deferred
  remote ACP design.
- [ACP architecture](https://agentclientprotocol.com/get-started/architecture)
- [ACP session configuration](https://agentclientprotocol.com/protocol/session-config-options)
- [ACP updates and RFD status](https://agentclientprotocol.com/rfds/updates)
- [ACP Streamable HTTP/WebSocket RFD](https://agentclientprotocol.com/rfds/streamable-http-websocket-transport)
- [MCP transports](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [Official MCP Registry overview](https://modelcontextprotocol.io/registry/about)
