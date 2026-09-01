# Spec: the ACP controller runtime

Status: **proposed** · Date: 2026-09-01

This document defines the core runtime beneath BitRouter's ACP-facing products.
It is authoritative for session ownership, controller topology, capability
negotiation, harness endpoint configuration, native session identity, and
session-scoped routing.

It supersedes the following decisions in [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md):

- the single-session product boundary in §§2, 8.3, 11, and 12;
- the use of standard ACP `providers/*` methods for BitRouter's internal route
  selection in §§6 and 10; and
- the assumption that one `Session` relay per manager connection is the right
  long-term ACP architecture in §5.

It does **not** supersede the shipped renderer, inline terminal behavior,
permission UI, cost presentation, or the `chat` command. Those become clients
of the controller defined here. The TUI is a product surface; the controller is
the product kernel.

---

## 1. Executive decision

BitRouter will own the **ACP control plane**, not the agent's conversation
state.

One BitRouter ACP controller instance launches and controls one agent harness
process. That connection may carry multiple harness-native sessions. The
controller configures the harness to send model traffic through BitRouter,
forwards the harness's ACP session lifecycle without replacing its identifiers,
and attaches enough controller and native-agent identity to model requests for
the router to attribute and isolate them accurately.

The harness remains the only authority for:

- session creation and native session identifiers;
- transcript and context persistence;
- history replay on load;
- resume, fork, close, and delete semantics;
- harness-specific modes, commands, plans, tools, and subagents; and
- the files or databases in which it stores those things.

BitRouter may hold an ephemeral live-session index for request dispatch,
cancellation, route-lease cleanup, and observability. It must not persist a
second session catalog, copy transcripts, edit harness session files, or mint a
replacement ACP session identifier.

The initial target is stable ACP v1 semantics over stdio. Draft capabilities,
especially custom LLM providers, are feature-detected and always have a pinned,
harness-specific fallback.

## 2. Why this is the boundary

ACP already assigns session ownership to the Agent. `session/new` returns an
Agent-generated session ID; `session/list` discovers sessions known to that
Agent; `session/load` asks the Agent to load and replay history; and
`session/resume` asks it to restore context without replay. Reimplementing any
of that in BitRouter would create two authorities for one conversation and
make native harness behavior less reliable, not more uniform.

Claude Code and Codex also already propagate useful native identity on their
model requests. The wrapper does not need to invent a workflow session. It
needs to preserve the ACP/native identity and make the router understand it.

This yields three deliberately separate planes:

```text
manager plane                 harness plane                 model plane

TUI / IDE / CLI  -- ACP -->  BitRouter ACP Controller  -- ACP -->  Harness
                                      ^                              |
                                      |                              |
                                      +---- BitRouter Core <--- HTTP-+
                                           route + meter
```

- The **manager plane** controls sessions and renders updates.
- The **harness plane** owns agent behavior and session data.
- The **model plane** routes and meters model requests using native session
  evidence plus authenticated controller correlation.

The controller connects these planes without collapsing their ownership.

## 3. Goals and non-goals

### 3.1 Goals

1. Programmatically point supported harnesses at BitRouter for provider, model,
   authentication, and static controller headers.
2. Preserve harness-native ACP session IDs end to end.
3. Forward all capability-gated ACP v1 session operations supported by the
   harness, including multiple concurrent sessions on one connection.
4. Negotiate capabilities honestly between the manager, controller, and
   harness instead of initializing the harness with empty client capabilities.
5. Extract Claude and Codex native session, thread, agent, parent, and turn
   signals at the BitRouter model ingress.
6. Apply BitRouter route overrides at native-session scope without changing
   the harness's session data.
7. Provide a reusable controller API used by `acp serve`, `chat`, and future
   ACP clients without depending on TUI code.
8. Fail visibly when a harness cannot be configured, a capability cannot be
   forwarded, or a request cannot be attributed.

### 3.2 Non-goals

- A BitRouter transcript store, session database, or session-file editor.
- A cross-harness session catalog. A controller instance exposes the sessions
  known to its one harness; a future harness supervisor may aggregate several
  controllers without taking ownership of their sessions.
- Automatic harness selection. Agent/harness routing is a separate policy and
  evaluation problem layered above this controller.
- ACP v2 wire semantics.
- A network ACP transport. Stdio remains the v1 transport; a future transport
  must not change the controller's ownership rules.
- Making every harness expose identical capabilities.
- Exact ACP-prompt-to-HTTP-request correlation when a harness does not expose a
  native turn identifier. Codex currently does; Claude Code currently does not
  document one on its model HTTP request.
- Persisting a session's temporary BitRouter route override across controller
  restarts. Persistence, if later desired, is router configuration and must be
  designed separately from harness session storage.

## 4. Terminology and identity model

The word `session` is overloaded today. New code and telemetry must use the
following names explicitly.

| Name | Owner | Lifetime | Meaning |
|---|---|---|---|
| `controller_instance_id` | BitRouter | one harness process / ACP connection | Correlates traffic with the controller that configured the harness |
| `acp_session_id` | harness | harness-defined | Opaque ID returned by ACP and used for every session method |
| `root_session_id` | harness | harness-defined | Root conversation identity observed on model HTTP traffic |
| `agent_thread_id` | harness | harness-defined | Child agent or thread identity within the root session |
| `parent_agent_thread_id` | harness | harness-defined | Native lineage when supplied |
| `turn_id` | harness | harness-defined | Native turn identity when supplied |
| `router_request_id` | BitRouter | one model request | BitRouter ingress and metering correlation |
| `route_lease_id` | BitRouter | one live controller/session override | Authorizes and cleans up an ephemeral route override |

### 4.1 Identity invariants

1. `acp_session_id` is opaque. BitRouter never parses, rewrites, aliases, or
   derives another manager-facing ID from it.
2. The ID returned by the harness is the ID returned to the manager.
3. An ID supplied by the manager is forwarded to the harness even if the
   current controller process has not seen it before. The harness, not an
   in-memory BitRouter map, decides whether it exists.
4. `controller_instance_id` must never be placed in the ACP `sessionId` field.
5. Native request evidence wins over compatibility headers. Conflicts are
   diagnostic events, not a reason to overwrite native identity.
6. Missing identity never causes BitRouter to fabricate a confident session.
   The request uses the default route and is marked unattributed.
7. Raw native IDs may exist in request scope and live controller memory.
   Existing metering and trace retention policy governs any diagnostic copy;
   the controller itself introduces no persistent session store.

### 4.2 Current harness mappings

| Harness | ACP ID returned by maintained adapter | Root request identity | Child identity | Turn identity |
|---|---|---|---|---|
| Claude Agent ACP | Claude native session UUID | `x-claude-code-session-id` | `x-claude-code-agent-id`, `x-claude-code-parent-agent-id` | not documented on model HTTP |
| Codex ACP | Codex `thread/start` thread ID | `session-id` | `thread-id`, parent/subagent fields in turn metadata | `turn_id` in Codex metadata |

For the current Claude adapter, the ACP session ID and Claude native session ID
are the same value. For a root Codex thread, `session-id` and `thread-id` are
normally equal; a fork or child may keep the root `session-id` while changing
`thread-id`. Router code must read these fields rather than infer the
relationship from string shape.

## 5. Hard architectural invariants

These are review gates, not preferences.

### 5.1 Harness session ownership

- No BitRouter database table or file stores ACP session records or transcript
  content.
- `session/list`, `load`, `resume`, `fork`, `close`, and `delete` are delegated
  to the harness.
- BitRouter does not inspect or mutate Claude's JSONL files, Codex's session
  files, or equivalent harness storage.
- The controller does not set `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or an
  equivalent home override unless the user explicitly requests an isolated
  harness environment. Test probes may use temporary homes.

### 5.2 Transparent session identifiers

- The existing manager-facing `record_id` alias is removed from the ACP wire.
- `agentSessionId` in optional `_meta` is treated only as additional evidence;
  it is not required and does not supersede `response.sessionId`.
- Session notifications, prompts, cancellations, and responses carry the same
  native ID in both directions.

### 5.3 Honest capability negotiation

- The controller advertises only methods it can forward or implement.
- The harness receives only client capabilities available from the manager or
  safely terminated by the controller.
- Draft features are enabled at runtime only when the harness advertises them.
- Unsupported methods return the protocol-defined error; they do not silently
  succeed or mutate only BitRouter's local view.

### 5.4 Protocol namespaces retain their meaning

- Harness-side ACP `providers/*` configures the harness's LLM endpoint.
- BitRouter route selection uses `_bitrouter/route/*` extensions.
- Standard `providers/*` is not advertised manager-side as a synonym for
  BitRouter's internal provider catalog.

### 5.5 Authenticated routing scope

- A raw `x-bitrouter-controller-id` header is correlation evidence, not
  authorization.
- The harness uses a controller-scoped BitRouter credential. The daemon derives
  the trusted controller identity from that credential and checks any claimed
  header against it.
- A session override is keyed by authenticated controller identity plus the
  opaque ACP session ID. Model ingress matches source-specific native identity
  candidates to that ID. Two controllers or two sessions cannot change each
  other's routes.

### 5.6 The controller has no TUI dependency

- `bitrouter-sdk` ACP controller code does not depend on `bitrouter-tui`.
- The TUI learns session state solely through ACP and BitRouter's documented
  extensions.
- Every controller behavior is testable through a headless manager.

## 6. Component architecture

The implementation is split into generic ACP mechanics and BitRouter product
policy.

### 6.1 `bitrouter-sdk`: protocol controller

The SDK owns protocol-correct, harness-independent behavior:

- harness process transport and the harness-side ACP connection;
- manager-side ACP serving;
- initialization sequencing and capability composition;
- multi-session request/notification forwarding;
- in-flight prompt and cancellation correlation;
- permission, elicitation, filesystem, terminal, and other client callbacks
  when supported by both sides;
- forwarding raw extension messages without losing fields; and
- lifecycle hooks that the app can use for endpoint configuration and route
  cleanup.

The current `Session` abstraction in `engine.rs` mixes one conversation, one
harness connection, a manager-facing alias, and recording state. It should be
split conceptually into:

```text
AcpController                 one harness process and ACP connection
  ├─ UpstreamAgentConnection  typed/raw RPC to the harness
  ├─ ManagerAgentEndpoint     typed/raw RPC exposed to the manager
  ├─ LiveSessionIndex         ephemeral dispatch and cleanup metadata
  └─ ControllerHooks          app-owned configuration and routing callbacks
```

`LiveSessionIndex` is not authoritative storage. Its minimum permitted state
is:

- native `acp_session_id` values observed during this process;
- in-flight prompt/cancel routing;
- optional `route_lease_id` for cleanup; and
- lifecycle state needed to prevent duplicate local work.

It must not contain transcript messages, replay history, derived summaries, or
a durable session catalog. An unknown session ID is forwarded rather than
rejected locally.

### 6.2 `apps/bitrouter`: harness and router integration

The application owns BitRouter-specific policy:

- the harness catalog and pinned adapter versions;
- per-harness launch fallback configuration;
- post-initialize `providers/set` configuration;
- controller credentials and static metadata headers;
- `_bitrouter/route/*` request handling;
- daemon route-lease installation and removal;
- Claude/Codex native identity extraction at HTTP ingress; and
- router-measured usage updates.

No Claude- or Codex-specific environment variable belongs in the generic SDK.
No ACP framing or session forwarding belongs in the TUI.

### 6.3 `bitrouter-tui`: optional manager client

The existing TUI remains a standalone ACP client. It may add session listing,
loading, resuming, closing, and deleting only by calling the corresponding ACP
methods and honoring advertised capabilities. It must not read harness homes or
invent a separate session model.

The first controller version does not require a session browser in the TUI.
Headless protocol conformance is the acceptance gate.

## 7. Controller lifecycle

### 7.1 Launch and initialize

The required sequence is:

1. The app resolves an explicit harness and creates a
   `controller_instance_id` plus a controller-scoped BitRouter credential.
2. The app builds one `HarnessEndpointPlan` containing the BitRouter base URL,
   logical model, authentication, and static headers.
3. The app launches the pinned adapter with the plan's harness-specific
   fallback environment already installed.
4. The manager sends ACP `initialize` to the controller.
5. The controller composes harness-side client capabilities from manager
   capabilities plus capabilities it safely terminates itself.
6. The controller initializes the harness exactly once.
7. If the harness advertises custom LLM providers, the app applies the same
   endpoint plan with harness-side `providers/set` and verifies it with
   `providers/list` when available.
8. The controller computes the honest manager-facing capability set and returns
   initialize success only after required endpoint configuration succeeds.
9. Session methods are accepted.

The adapter process may be spawned before manager initialization, but ACP
`initialize` must not be sent to it with placeholder or empty capabilities.

If an adapter that advertises provider configuration rejects the plan, startup
fails with a structured configuration error. The controller must not silently
continue on the hope that fallback environment variables happen to win.

If the adapter does not advertise provider configuration, the pinned adapter's
documented fallback is authoritative. This compatibility path is covered by a
live request-capture test for every supported adapter version.

### 7.2 Manager-facing agent information

The manager is connected to BitRouter, so `agentInfo` identifies the BitRouter
ACP controller. Sanitized upstream `agentInfo`, adapter package/version, and
harness ID are exposed under namespaced response `_meta` for diagnostics.
Secrets and full launch commands are excluded.

This avoids pretending the wrapper is absent while preserving enough upstream
information for troubleshooting and UI labels.

### 7.3 Normal shutdown

On manager disconnect or controller shutdown:

1. cancel or finish in-flight local forwarding according to ACP semantics;
2. remove every BitRouter route lease owned by the controller;
3. terminate the child adapter according to the existing process policy; and
4. discard the ephemeral live-session index.

The controller does **not** close or delete harness-native sessions merely
because the manager disconnected. Durable session availability after process
exit is whatever the harness natively guarantees.

## 8. Capability composition and ACP method surface

### 8.1 Composition rule

There are two related but different capability sets:

```text
harness-side client capabilities
  = manager client capabilities that the controller can forward
  + client capabilities the controller implements locally

manager-side agent capabilities
  = harness agent capabilities that the controller can forward
  + BitRouter namespaced extensions implemented by the controller
```

A field is never copied merely because both schemas contain the same name. Its
requests, responses, notifications, cancellation, and error behavior must all
be forwardable before it is advertised.

### 8.2 Stable session methods

| ACP operation | Controller behavior |
|---|---|
| `session/new` | Forward request; return harness response unchanged; install only ephemeral route scope before replying |
| `session/list` | Forward and return the harness's catalog; never merge with a BitRouter catalog |
| `session/load` | Forward; relay the harness's replay updates; learn the returned/used native ID |
| `session/resume` | Forward without synthesizing replay |
| `session/prompt` | Forward by native ID; relay all updates and return the harness stop result |
| `session/cancel` | Forward by native ID, including for a session first observed before controller restart |
| `session/close` | Forward; remove BitRouter live state and route lease only after harness success |
| `session/delete` | Forward; remove BitRouter live state and route lease only after harness success |
| `session/fork` | Forward when advertised; return the new native ID unchanged and install its own route scope |
| `session/set_config_option` | Forward when advertised; do not mirror harness configuration locally |

`session/list`, `load`, `resume`, `close`, `delete`, `fork`, and configuration
methods are capability-gated. The controller does not emulate a missing
harness capability with partial local state.

### 8.3 Agent-to-client callbacks and updates

The controller preserves all understood session updates, including message,
thought, tool, plan, command, configuration, state, and usage variants. Unknown
extension fields and `_meta` survive raw forwarding.

Permission and elicitation callbacks are forwarded only when the manager
advertises and the controller implements the complete round trip. Filesystem or
terminal callbacks follow the same rule. The controller may later terminate a
callback locally, but that is an explicit implementation with its own security
policy, not an assumed capability.

### 8.4 ACP `_meta`

ACP `_meta` is useful for manager-to-controller correlation and transparent
extension forwarding. It is not a generic transport into the harness's model
HTTP request. The controller therefore does not depend on prompt `_meta` to
deliver session identity to BitRouter Core.

## 9. Harness endpoint and model configuration

### 9.1 One canonical endpoint plan

All configuration mechanisms are rendered from one in-memory value so the
provider call and fallback environment cannot drift:

```text
HarnessEndpointPlan
  base_url             BitRouter model endpoint
  protocol             Anthropic Messages or OpenAI Responses
  logical_model        model or BitRouter preset presented to the harness
  controller_credential short-lived credential bound to the controller
  static_headers
    x-bitrouter-controller-id
    x-bitrouter-harness
```

The credential is passed through the adapter's secret-bearing configuration
field or environment variable. It is never returned from `providers/list`,
included in `_meta`, printed in logs, or rendered by the TUI.

`x-bitrouter-controller-id` and `x-bitrouter-harness` improve diagnostics, but
the daemon trusts the credential binding rather than those caller-controlled
strings.

### 9.2 Preferred path: harness-side `providers/set`

When the initialized harness advertises the provider capability, the
controller calls `providers/set` after initialization and before any
`session/new`, `load`, or `resume`. The configured provider points to BitRouter,
uses the required wire protocol, carries the controller credential and static
headers, and is process-scoped.

The controller verifies the effective non-secret URL/protocol/model through
`providers/list` when the adapter implements it. A mismatch is a startup error,
not a warning.

Because the ACP custom-provider RFD remains Draft, this path is always
feature-detected and version-tested. No manager-side provider capability is
implied by using it internally.

### 9.3 Claude fallback

The maintained adapter is `@agentclientprotocol/claude-agent-acp`. The current
`@zed-industries/claude-code-acp` catalog entry is deprecated and must be
replaced.

The fallback launch plan uses documented Claude settings:

- `ANTHROPIC_BASE_URL`;
- `ANTHROPIC_AUTH_TOKEN`;
- the appropriate Claude model environment setting; and
- newline-separated `ANTHROPIC_CUSTOM_HEADERS` for static BitRouter headers.

The adapter inherits these settings into the Claude Agent SDK/Claude Code
process. Product code must not redirect Claude's config home by default.

### 9.4 Codex fallback

The maintained `codex-acp` adapter launches Codex App Server. Its fallback
launch plan uses:

- `CODEX_CONFIG` JSON containing a custom Responses provider with BitRouter's
  `base_url`, secret source, and static headers; and
- `MODEL_PROVIDER` selecting that provider.

Arbitrary Codex CLI `-c` arguments appended to the `codex-acp` command are not
a supported adapter configuration path. The current `codex_overlay` behavior
must be removed for ACP mode; direct interactive Codex launch may keep its own
CLI-specific overlay.

Product code must not redirect `CODEX_HOME` by default.

### 9.5 Version policy

Adapter commands use exact versions or a checked lock, never bare `@latest`.
The research baseline on 2026-09-01 validated:

- `@agentclientprotocol/claude-agent-acp` 0.70.0 from the maintained ACP
  repository; and
- `@agentclientprotocol/codex-acp` 1.7.0 backed by Codex App Server.

Implementation starts from those exact package versions. If registry or
platform constraints require a different version, the compatibility change
must record the package version and upstream commit used by its conformance
tests and rerun endpoint, session, capability, and identity smokes.

## 10. Native identity at BitRouter model ingress

### 10.1 Normalized evidence

The existing workflow-state extractor boundary gains a source-independent
identity result:

```text
NativeAgentIdentity
  harness
  root_session_id?
  agent_thread_id?
  parent_agent_thread_id?
  turn_id?
  evidence_source
  confidence
```

This is request metadata, not an ACP session record. Runtime-specific parsing
stays inside the Claude and Codex extractors; routing and metering consume only
the normalized result.

### 10.2 Claude precedence

1. `x-claude-code-session-id`, `x-claude-code-agent-id`, and
   `x-claude-code-parent-agent-id`;
2. documented/native body metadata such as Claude's session-bearing user
   metadata;
3. legacy compatibility hints already accepted by BitRouter; and
4. unattributed generic fallback.

The current extractor's Anthropic beta detection remains useful for harness
recognition, but it is not a substitute for reading the native headers.

### 10.3 Codex precedence

1. `session-id` and `thread-id` request headers;
2. Codex turn metadata and Responses `client_metadata`, including `turn_id`,
   parent thread, and subagent fields;
3. `previous_response_id` and Codex user-agent as weaker compatibility
   evidence; and
4. unattributed generic fallback.

The extractor must not assume `session-id == thread-id`; it records the values
supplied by Codex.

### 10.4 Conflict behavior

If two native fields conflict, the extractor follows the source-specific
precedence above, attaches a structured diagnostic, and avoids using the lower
confidence value for route scope. A private BitRouter harness header can help
select an extractor only when no stronger native evidence identifies one; it
cannot replace a native session ID.

## 11. Session-scoped BitRouter routing

### 11.1 Separate endpoint configuration from route selection

Two controls that are currently conflated must become separate:

| Control | Direction | Meaning |
|---|---|---|
| ACP `providers/*` | controller → harness | Configure the harness's model endpoint to BitRouter |
| `_bitrouter/route/*` | manager → controller | Select how BitRouter routes one native harness session |

The first is an ACP provider capability. The second is a BitRouter product
extension. Reusing one namespace for both makes a generic ACP client believe it
is reconfiguring the Agent's endpoint when it is actually mutating an internal
router policy.

### 11.2 Extension surface

The v1 extension consists of:

```text
_bitrouter/route/list
  request:  { sessionId }
  response: { available, current, scope: "session" }

_bitrouter/route/set
  request:  { sessionId, route }
  response: { current, scope: "session" }

_bitrouter/route/reset
  request:  { sessionId }
  response: { current, scope: "default" }
```

`route` uses BitRouter's existing non-secret route vocabulary: preset, logical
model, or explicit provider/model where allowed by router policy. Credentials
never cross this surface.

The controller acknowledges `set` only after the daemon confirms that the
route lease is installed. `list` reports the effective route from the daemon,
not a controller-local optimistic value.

### 11.3 Route key and descendant behavior

The trusted daemon key is:

```text
(authenticated_controller_instance_id, acp_session_id)
```

The daemon does not expect the HTTP client to send a field literally named
`acp_session_id`. The native extractor produces ordered lookup candidates:

| Harness | Route lookup candidates |
|---|---|
| Claude | `x-claude-code-session-id` |
| Codex | `thread-id`, then `session-id` |

For a root Codex session, both values normally match the ACP thread ID. For an
ACP fork, the new `thread-id` can match its own route lease before falling back
to the root `session-id`. For a subagent thread that is not separately exposed
as an ACP session, no exact lease exists and the root session lease is
inherited. Claude child agents inherit the lease selected by the Claude session
ID. Explicit per-subagent route controls are out of scope for v1.

The manager can install the route immediately after `session/new`, `load`,
`resume`, or `fork` returns because it already has the native ID. The first
prompt therefore does not race route installation.

If a model request has authenticated controller identity but no native root
session identity, no session override applies. The default router policy runs
and telemetry marks the request unattributed. If it has a native ID but an
invalid controller credential, it cannot access another controller's lease.

### 11.4 Lease lifetime

A route override is an ephemeral `route_lease`:

- created or replaced by `_bitrouter/route/set`;
- removed by `reset`;
- removed after successful `session/close` or `session/delete`;
- removed when the controller disconnects or its credential expires; and
- not restored automatically by harness `session/resume` after a new
  controller process starts.

This is routing control state, not conversation state. A future persistent
route preference must use an explicit router configuration feature and cannot
be hidden inside session management.

### 11.5 Migration from manager-side `providers/*`

The shipped TUI is migrated to `_bitrouter/route/*` in the same release as the
new controller. During one compatibility window, raw manager-side
`providers/list` and `providers/set` calls may be accepted without advertising
the capability and must return a deprecation marker in response `_meta`.

After the compatibility window, manager-side standard `providers/*` is absent
by default. A future explicit endpoint-administration mode may expose the
standard meaning, but it is a separate security-reviewed feature. It never
remains an alias for internal routing.

## 12. Session flows

### 12.1 New session

```text
Manager                  Controller                 Harness          Daemon
   | session/new             |                         |                |
   |------------------------>| session/new             |                |
   |                         |------------------------>|                |
   |                         |<------------------------| native id      |
   |                         | register live scope -------------------->|
   |<------------------------| same native id          |                |
```

Registration creates only the authenticated scope needed for later route
leases. It does not create a transcript record.

### 12.2 List, load, and resume

`session/list` is a direct query to the harness. The controller may stream and
forward the response but does not cache it as a catalog.

For `session/load`, replay updates emitted by the harness are forwarded to the
manager. BitRouter neither supplies nor edits that history. `session/resume`
restores the harness context without the controller synthesizing replay.

After load/resume success, the controller registers ephemeral route scope for
the native session before returning success. A route preference from an old
controller is not implied.

### 12.3 Prompt and model request

```text
Manager        Controller        Harness             BitRouter Core
   | prompt(id)    |                |                       |
   |-------------->| prompt(id)     |                       |
   |               |--------------->| model HTTP            |
   |               |                |---------------------->|
   |               |                | controller credential |
   |               |                | native session fields |
   |               |                |                       | resolve lease
   |               |                |<----------------------|
   |<-- updates ---|<---------------|                       |
```

There is no controller-generated session header in this flow. Static
controller correlation comes from endpoint configuration; session identity
comes from the harness's native model client.

### 12.4 Close, delete, and disconnect

The controller removes local route state only after the harness accepts close
or delete. A failed harness operation leaves the route lease in place until an
explicit reset, controller teardown, or credential expiry.

An ACP connection drop removes controller-owned leases but does not translate
to harness `session/delete` or erase harness data.

## 13. Error model

Errors must identify which plane failed and must not leave the UI showing a
control that only changed local display state.

| Failure | Required behavior |
|---|---|
| Adapter cannot start | Structured controller startup error; include sanitized command/package and log path |
| ACP version mismatch | Fail initialize before any session operation |
| Advertised `providers/set` fails | Fail initialize; do not silently prefer fallback |
| No provider capability | Use the pinned adapter fallback and record that path |
| Effective provider mismatches endpoint plan | Fail initialize when verification is available |
| Harness session method fails | Preserve harness ACP error and native ID; do not mutate local catalog |
| Manager requests unsupported method | Protocol-defined unsupported/method-not-found response |
| Route lease installation fails | `_bitrouter/route/set` fails; `current` remains daemon-confirmed state |
| Native model identity missing | Use default route; emit unattributed diagnostic; never fabricate ID |
| Native identity conflict | Use source-specific native precedence and emit conflict diagnostic |
| Daemon unavailable | Model request fails through the normal provider path; controller surfaces relevant harness error/update |
| Adapter exits | Fail in-flight operations, remove route leases, retain harness-owned session files untouched |

Agent stderr and BitRouter controller diagnostics continue to use the existing
per-controller/session log facilities. Secrets, provider tokens, authorization
headers, and full provider configuration payloads are redacted at the source.

## 14. Security and privacy

1. Controller credentials are short-lived, least-privilege, and bound to one
   controller instance. Reusing a user's upstream model credential as the
   controller credential is forbidden.
2. The daemon authenticates route-control commands and model requests. Header
   strings alone do not grant access to a route lease.
3. Provider configuration responses expose only non-secret effective fields.
4. Session IDs are treated as opaque identifiers. They are not placed in error
   messages that may be sent to an unrelated manager connection.
5. The controller never reads harness session-storage directories merely to
   populate UI or router state.
6. Existing request tracing may associate metering with normalized agent
   identity, but it must not reinterpret that telemetry as a resumable session
   store.
7. Custom base URLs and headers follow the existing BitRouter configuration
   trust model. Any future remote controller transport requires a separate
   authentication and threat-model review.

## 15. Observability

Each controller and model request should expose structured, non-secret fields:

- controller instance and harness ID;
- adapter name/version and whether endpoint configuration used ACP provider or
  fallback mode;
- ACP method, native session ID presence, and capability decision;
- normalized root/thread/parent/turn identity presence;
- router request ID, effective route, route scope, and route-lease outcome;
- identity conflicts or unattributed reasons; and
- process exit and cleanup outcomes.

Session identifiers may be transformed according to the existing telemetry
privacy policy, but the transformation must remain stable enough to group one
native session accurately. Message text, transcript history, and harness
session-file paths are not new controller telemetry.

The controller exposes no claim of session-level cost when the model request
cannot be attributed. Existing `UsageUpdate.cost` behavior must carry an honest
scope marker and may report unknown rather than daemon-wide data as if it were
session data.

## 16. Compatibility and migration

### 16.1 ACP SDK

The current workspace pins `agent-client-protocol` 1.2 and schema 1.4. The
implementation should upgrade to the current stable-v1 Rust SDK/schema as part
of the controller refactor, while keeping ACP v1 wire behavior. ACP v2 draft
types are not enabled merely because the Rust SDK major version changes.

### 16.2 Existing one-session `Session`

The current `record_id`-based `Session` path remains only until the controller
passes equivalent prompt/update/cost tests. It is not used as a compatibility
layer around native IDs because doing so would preserve the central defect.

Migration should keep reusable turn execution, update translation, recording,
and metering components, but move them under a connection-level controller
that can serve multiple sessions.

### 16.3 Existing commands

- `bitrouter acp serve <harness>` evolves into the headless stdio entry point
  for the multi-session controller; its explicit harness argument remains.
- `bitrouter chat <harness>` uses the same controller library as an in-process
  manager client.
- Direct `bitrouter launch` remains the harness-owned native UX and is
  unaffected.
- No TUI command is required to prove controller correctness.

### 16.4 Adapter catalog

- Replace the deprecated Claude ACP package and its package markers.
- Split direct Codex CLI overlay from Codex ACP adapter configuration.
- Pin adapter versions and cache/install them through the existing harness
  mechanisms.
- Keep adapter-specific fallback configuration in catalog/application code,
  not generic ACP modules.

### 16.5 Verified delta from current `main`

The design is a controller refactor, not a description of the current relay:

- [`engine.rs`](../crates/bitrouter-sdk/src/acp/engine.rs) mints a BitRouter
  `record_id`, stores the upstream IDs behind a one-time cell, and makes
  repeated `session/new` calls return the same manager-facing ID.
- [`down.rs`](../crates/bitrouter-sdk/src/acp/down.rs) deliberately prevents the
  upstream ID from crossing the manager boundary and masks native load
  semantics.
- [`up.rs`](../crates/bitrouter-sdk/src/acp/up.rs) initializes the harness with
  default client capabilities before the manager's capabilities are known and
  looks only for optional `_meta.agentSessionId` as the provider-native ID.
- [`harness.rs`](../apps/bitrouter/src/harness.rs) launches the deprecated
  Claude adapter with `@latest` and appends direct Codex CLI `-c` settings to
  the Codex ACP adapter. A live adapter probe showed those `-c` settings do not
  change the adapter's App Server provider; harness-side `providers/set` does.
- [`acp_cli.rs`](../apps/bitrouter/src/acp_cli.rs) implements manager-side
  `providers/list` and `providers/set` as BitRouter route controls and scopes
  the daemon override with a launch ID. The behavior works for the shipped
  one-session TUI but has the wrong protocol meaning and process-level scope
  for a multi-session controller.
- The current
  [Claude extractor](../apps/bitrouter/src/workflow_state/extractors/claude_code.rs)
  does not consume `x-claude-code-*` identity headers, and the
  [Codex extractor](../apps/bitrouter/src/workflow_state/extractors/codex.rs)
  does not consume current `session-id`, `thread-id`, or Codex turn metadata.

The feasibility probes used temporary harness homes, dummy credentials, and a
local capture endpoint. Claude Code emitted its native session header plus the
configured BitRouter header. Codex emitted native session/thread headers,
turn/client metadata, and the configured BitRouter header. No user session was
loaded and no real upstream model request was made.

## 17. Implementation boundaries in the current tree

The design is expected to touch these boundaries; exact file splits belong in
the implementation plan.

| Area | Current files | Intended change |
|---|---|---|
| Controller lifecycle | `crates/bitrouter-sdk/src/acp/engine.rs`, `up.rs`, `down.rs`, `session.rs` | Replace one-session aliasing with connection-level, multi-session forwarding |
| Capabilities | `crates/bitrouter-sdk/src/acp/up.rs`, `down.rs` | Manager-first composition and honest intersection |
| Harness configuration | `apps/bitrouter/src/harness.rs`, `acp_cli.rs` | Maintained adapters, pinned versions, endpoint plan, provider/fallback paths |
| Route extensions | `apps/bitrouter/src/acp_cli.rs`, daemon/router control modules | `_bitrouter/route/*` and authenticated native-session leases |
| Claude identity | `apps/bitrouter/src/workflow_state/extractors/claude_code.rs` | Native Claude header precedence and lineage |
| Codex identity | `apps/bitrouter/src/workflow_state/extractors/codex.rs` | Native session/thread/turn metadata precedence |
| TUI migration | `crates/bitrouter-tui`, `apps/bitrouter/src/chat` | Consume session methods and namespaced route control only |

The implementation must not create an app dependency from the SDK or a TUI
dependency from the controller.

## 18. Verification strategy

### 18.1 Unit tests

- capability composition for every manager/harness support combination;
- native session ID pass-through, including IDs unknown to the live index;
- no fallback to `record_id` or `_meta.agentSessionId` as the canonical ID;
- Claude and Codex identity precedence, missing fields, and conflicts;
- route keys derived from authenticated controller plus native root session;
- secret redaction from provider responses, logs, and errors; and
- route-lease cleanup state transitions.

### 18.2 Protocol contract tests with fake harnesses

Drive raw JSON-RPC over stdio and prove:

1. manager initialize capabilities affect harness initialize capabilities;
2. manager-facing capabilities are an honest composition;
3. `providers/set` runs after initialize and before the first session method;
4. two sessions can prompt concurrently on one harness connection;
5. list/load/resume/fork/close/delete are forwarded and preserve native IDs;
6. replay updates from load reach the manager unchanged;
7. unknown update variants and `_meta` survive forwarding;
8. permission/callback behavior matches negotiated capabilities; and
9. controller disconnect removes route leases without issuing session delete.

### 18.3 Router integration tests

- Start two native sessions under one controller, install different routes,
  and prove their model requests resolve independently.
- Start two controllers with equal-looking native session IDs and prove the
  controller credential prevents collision.
- Prove Claude child agents and Codex child threads inherit the root route.
- Prove an unattributed request uses the default route and cannot claim a
  session override.
- Prove `_bitrouter/route/set` reports success only after daemon installation.
- Prove session close/delete/disconnect removes only the intended lease.

### 18.4 Live adapter conformance

For each pinned Claude and Codex adapter, use a local capture server and dummy
credentials to prove without touching the user's harness home:

- the configured request reaches the BitRouter URL;
- the controller credential and static headers arrive;
- native session/root/thread/agent/turn fields arrive as documented;
- `session/new`, list, load, and resume work when advertised;
- two sessions work in one adapter process;
- provider configuration or its documented fallback is the path actually used;
  and
- no real upstream token is spent.

Live tests may be release-gated rather than run in every offline CI job, but
their fixtures and expected evidence are versioned.

### 18.5 Workspace gates

The implementation plan must finish with:

- `cargo nextest run --all-features`;
- `cargo clippy --workspace --all-features --tests -- -D warnings`;
- `cargo fmt -- --check`; and
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`.

## 19. Acceptance criteria

The controller kernel is complete when all of the following are true:

1. A generic headless ACP manager can initialize BitRouter and create two
   concurrent sessions against one pinned Claude or Codex adapter process.
2. Every manager-visible session ID is exactly the harness-returned ID.
3. A fresh controller can list/load/resume a harness-native session without a
   BitRouter session database.
4. Capability-dependent methods appear only when the complete path works.
5. Claude and Codex model requests reach BitRouter with authenticated controller
   correlation and native root session identity.
6. Two sessions select different BitRouter routes without cross-talk.
7. Standard ACP `providers/*` is used for harness endpoint configuration, not
   advertised as BitRouter route selection.
8. The TUI can perform its existing prompt, permission, cost, and route actions
   solely through ACP plus `_bitrouter/route/*`.
9. Restarting or disconnecting the controller loses only ephemeral BitRouter
   route state; harness-native sessions remain governed by the harness.
10. No test or production path reads, copies, or mutates native session files.
11. No provider credential appears in ACP output, logs, errors, or TUI state.
12. Full workspace verification passes on the final tree.

## 20. Delivery phases

These phases define scope boundaries, not an executable task plan.

### Phase 0 — compatibility foundation

- upgrade the ACP Rust SDK while retaining stable v1 wire semantics;
- migrate and pin the Claude adapter;
- pin the Codex adapter;
- replace Codex ACP `-c` routing with endpoint-plan configuration; and
- add adapter endpoint/identity smoke fixtures.

### Phase 1 — controller kernel

- introduce the connection-level controller;
- compose capabilities manager-first;
- preserve native IDs;
- forward the complete capability-gated session lifecycle; and
- support multiple sessions per connection.

### Phase 2 — identity and route isolation

- inject authenticated controller configuration;
- parse Claude and Codex native request identity;
- introduce session route leases and `_bitrouter/route/*`;
- migrate the TUI off manager-side `providers/*`; and
- remove the compatibility alias after its stated window.

### Phase 3 — product surfaces

- add harness-native session browsing and lifecycle controls to the TUI if the
  product requires them;
- improve controller diagnostics and recovery UX; and
- validate third-party ACP managers against the same controller surface.

Automatic harness selection is a later, separate design. It consumes explicit
harness capabilities and evaluation evidence; it does not change session
ownership or the controller contract.

## 21. Risks and mitigations

| Risk | Consequence | Mitigation |
|---|---|---|
| ACP provider configuration changes while Draft | Adapter upgrades break automatic endpoint setup | Feature detection, exact adapter pins, one canonical endpoint plan, tested fallbacks |
| Adapter capabilities differ or drift | UI exposes controls that fail | Honest runtime composition plus adapter conformance fixtures |
| Native request metadata changes | Session route or analysis loses attribution | Source-specific extractors, multiple evidence levels, conflict diagnostics, default-route fallback |
| Route state is keyed only by process/launch | One session changes another | Authenticated `(controller, root session)` lease key and two-session tests |
| Controller becomes another session database | Native resume diverges from BitRouter state | Hard no-storage invariant and restart/load acceptance test |
| TUI requirements leak into protocol core | Controller becomes hard to embed/test | SDK/TUI dependency boundary and headless acceptance gate |
| Exact Claude turn correlation is unavailable | ACP prompt cannot be tied to one HTTP call with certainty | Treat session correlation as sufficient for v1; add adapter extension only if a measured use case requires turn-level precision |

## 22. Rejected alternatives

### 22.1 Keep the manager-facing `record_id`

Rejected because it creates a BitRouter session identity that has to be mapped,
stored, and recovered separately from the harness. It also prevents generic
clients from using native list/load/resume semantics directly.

### 22.2 Store a shadow session catalog in BitRouter

Rejected because freshness, deletion, fork lineage, and replay would have two
authorities. A cache that is not authoritative is unnecessary for v1; a cache
that is authoritative violates the product boundary.

### 22.3 Continue using `providers/*` as the route picker

Rejected because current ACP provider semantics configure the Agent's LLM
endpoint. BitRouter route selection is a different operation with different
scope, authorization, and lifecycle.

### 22.4 Depend only on prompt `_meta` for model correlation

Rejected because ACP metadata forwarding does not guarantee propagation into a
harness's model HTTP request. Native Claude/Codex request identity is both more
accurate and already available.

### 22.5 One harness process per session

Rejected as the controller architecture because ACP permits multiple sessions
per connection and both target adapters implement multi-session lifecycle
operations. Process-per-session may remain a diagnostic fallback, not the
default or public ownership model.

### 22.6 Build the session manager in the TUI first

Rejected because it would encode protocol and storage assumptions in a
presentation layer. The TUI can only be a faithful session UI after the
controller transparently exposes harness-native lifecycle operations.

## 23. Authoritative references

- [ACP architecture](https://agentclientprotocol.com/get-started/architecture)
- [ACP initialization](https://agentclientprotocol.com/protocol/v1/initialization)
- [ACP session setup and lifecycle](https://agentclientprotocol.com/protocol/v1/session-setup)
- [ACP session listing](https://agentclientprotocol.com/protocol/v1/session-list)
- [ACP extensibility and metadata](https://agentclientprotocol.com/protocol/v1/extensibility)
- [ACP metadata propagation RFD](https://agentclientprotocol.com/rfds/meta-propagation)
- [ACP custom LLM endpoint RFD](https://agentclientprotocol.com/rfds/custom-llm-endpoint)
- [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway)
- [Claude Code environment variables](https://code.claude.com/docs/en/env-vars)
- [Claude Code sessions](https://code.claude.com/docs/en/sessions)
- [Claude Agent ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp)
- [Codex configuration reference](https://developers.openai.com/codex/config-reference)
- [Codex App Server](https://developers.openai.com/codex/app-server)
- [Codex request identity headers](https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/requests/headers.rs)
- [Codex Responses metadata](https://github.com/openai/codex/blob/main/codex-rs/core/src/responses_metadata.rs)
- [Codex ACP adapter](https://github.com/agentclientprotocol/codex-acp)
