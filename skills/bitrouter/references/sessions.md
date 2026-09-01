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
BitRouter route picker. Session-scoped BitRouter route controls are a later,
namespaced controller feature.

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

The model-router ingress continues to observe ordinary model API requests.
Routed adapter requests carry the controller and harness headers above, while
Claude and Codex emit their own native session/thread headers. The P1
controller does not synthesize manager-facing per-session cost updates or
store session telemetry; normalized authenticated native-session correlation
and session route leases belong to the subsequent identity/routing phase.

## One-shot NDJSON

`acp prompt`/`spawn -p` emits a first `session` line with its local `record_id`
and `via`, followed by `message_chunk`, `thought_chunk`, `tool_call`,
`tool_call_update`, and `usage` lines, then a `result` line. `--no-wait` emits
`submitted`. This format belongs to the single-session engine only; it is not
the `acp serve` wire format.
