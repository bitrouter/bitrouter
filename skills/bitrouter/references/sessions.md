# ACP controller and one-shot sessions

How BitRouter's ACP surfaces divide ownership. For CLI flags see
`references/cli.md` §ACP sessions; for adapter config see
`references/providers.md` §ACP agents.

## Controller vs one-shot engine

`bitrouter acp serve` is a connection-level ACP controller:

```text
manager -- ACP --> BitRouter controller -- ACP --> one harness process
                         connection carries N harness-native sessions
```

One controller process owns one live harness connection, not one conversation.
The manager may call `session/new` repeatedly and may list, load, resume, fork,
close, or delete sessions when the harness advertises those capabilities.
Every manager-visible `sessionId` is the opaque ID returned by the harness.
BitRouter does not generate an alias, store a session catalog or transcript,
or read Claude/Codex session files.

`bitrouter acp prompt` and `bitrouter chat` still use the local single-session
engine. That engine owns a local `record_id`, a FIFO turn queue, cooperative
cancellation, optional `--turn-timeout`, telemetry, and the interactive route
surface. Do not project those local-engine semantics onto `acp serve`.

## Controller launch and initialization

```bash
# Manager-driven, multiple native sessions on one harness connection
bitrouter acp serve --agent <id> [--config PATH]

# Equivalent umbrella command
bitrouter spawn <id> --serve [routing flags]
```

Stdout is ACP JSON-RPC and logs go to stderr. The manager sends `initialize`
first. BitRouter forwards the manager's client capabilities and `_meta` to the
harness, initializes the harness exactly once, configures its BitRouter model
endpoint when supported, then returns initialize success. Manager-facing
`agentInfo` identifies `bitrouter-acp-controller`; sanitized harness and pinned
adapter identity are under `_meta["bitrouter.dev/controller"]`.

The controller passes through harness lifecycle capabilities, but removes the
internal custom-provider capability. Standard `providers/*` configures the
harness endpoint from controller to harness; it is not a manager-side
BitRouter route picker. The connection uses stable ACP v1 wire semantics; the
Rust runtime crate's major version is not an ACP wire-version selector.

When the controller has a local daemon route-control backend, initialize metadata
advertises `_meta["bitrouter.dev/controller"].routeControl` with
`version: "1"`, `scope: "session"`, and these methods:

```text
_bitrouter/route/list   { sessionId }
_bitrouter/route/set    { sessionId, route }
_bitrouter/route/reset  { sessionId }
```

The manager must capability-probe this metadata. An absent or null
`routeControl` means the route UI is unavailable, and calling the extension
returns method-not-found. `list` and `set` are daemon-confirmed; `route` accepts
BitRouter presets, logical models, or explicit provider/model routes allowed
by current policy. `list.available` contains live logical-model picker
suggestions, not an exhaustive grammar for presets or explicit routes. Do not
use manager-side `providers/*` as a compatibility alias.

## Pinned Claude and Codex adapters

The maintained catalog commands are exact pins:

```bash
npx -y @agentclientprotocol/claude-agent-acp@0.70.0
npx -y @agentclientprotocol/codex-acp@1.7.0
```

When routing is active, one endpoint plan drives both provider setup and its
launch fallback:

- Claude: provider id `main`, Anthropic protocol,
  `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, optional `ANTHROPIC_MODEL`,
  and newline-separated `ANTHROPIC_CUSTOM_HEADERS`.
- Codex: provider id `openai`, OpenAI Responses protocol, `CODEX_CONFIG` JSON
  plus `MODEL_PROVIDER`. ACP mode does not append Codex `-c` arguments.

Both plans include non-secret `x-bitrouter-controller-id` and
`x-bitrouter-harness` headers plus secret authorization. Secrets are never
placed in ACP metadata, provider verification output, logs, or errors.
`--direct` skips the endpoint plan and uses the harness's own provider auth.

## Transparent native lifecycle

The controller forwards these methods without requiring that it has seen the
session ID before:

- `session/new`, `list`, `load`, `resume`, `fork`, `close`, and `delete`;
- `session/prompt`, `session/cancel`, and `session/set_config_option`;
- every session update and harness-authored response/error; and
- permission, filesystem, terminal, and extension callbacks supported by the
  manager.

Requests, responses, notifications, `_meta`, and unknown extension payloads
pass through without a BitRouter session alias. On manager disconnect the
harness child is terminated and live controller state is discarded; the
controller does not close or delete harness sessions. Whether a session is
durable is entirely the harness's native behavior.

## Routing and observability boundary

Routing is attempted by default for supported catalog adapters. Use `--direct`
to opt out, `--model` to pin the logical model, `--base-url` to select a daemon,
and `--no-start` to disable local daemon auto-start. Routing/auth failures occur
before the ACP handshake.

When API authentication is enabled, local routing uses the normal BitRouter
API/virtual key for both model requests and the route principal. Under
`skip_auth: true`, both use the deliberately shared `local` principal. The
owner-only daemon socket carries route mutations but does not mint or validate
a second controller credential. An explicit remote `--base-url` can still
configure the harness's model endpoint, but does not advertise route controls
until hosted HTTP route control exists.

The model-router ingress continues to preserve ordinary model API session
parsing. Routed adapter requests normalize caller-declared BitRouter
controller/harness headers together with Claude or Codex native
session/thread/agent/turn evidence. Session routes are ephemeral leases keyed
by API principal, declared controller, and native session. These headers are
correlation and routing claims, not authenticated facts; processes sharing one
API key can deliberately reuse them. An explicit caller route or preset and a
Responses continuation pin remain stronger than a lease.

Close/delete removes a lease only after the harness operation succeeds;
disconnect, lease expiry, reset, daemon restart, and controller cleanup also
remove it. None of these operations changes harness session storage. The normalized
identity event joins controlled capture/replay, spans, route decisions, and
nullable metering columns by `router_request_id`; authorization, cookies, and
credentials are excluded, and raw identifiers are never aggregate metric
labels. The controller does not synthesize manager-facing per-session cost.

## One-shot NDJSON

`acp prompt`/`spawn -p` emits a first `session` line with its local `record_id`
and `via`, followed by `message_chunk`, `thought_chunk`, `tool_call`,
`tool_call_update`, and `usage` lines, then a `result` line. `--no-wait` emits
`submitted`. This format belongs to the single-session engine only; it is not
the `acp serve` wire format.
