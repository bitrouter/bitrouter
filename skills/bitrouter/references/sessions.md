# ACP controller and native sessions

How BitRouter's ACP surfaces divide ownership. For CLI flags see
`references/cli.md` §ACP sessions; for adapter config see
`references/providers.md` §ACP agents.

## One controller, three drivers

`bitrouter acp serve` is a connection-level ACP controller:

```text
manager -- ACP --> BitRouter controller -- ACP --> one harness process
                         connection carries N harness-native sessions
```

One controller process owns one live harness connection, not one conversation.
The manager may call `session/new` repeatedly and may list, load, resume, fork,
close, or delete sessions when the harness advertises those capabilities.
Every manager-visible `sessionId` is the opaque ID returned by the harness.
BitRouter does not generate an alias or replace the harness's session catalog.
Maintained Codex and Claude adapters also collect local evaluation evidence:
original ACP session envelopes, native lifecycle observations, and explicitly
related native transcript files. This evidence index never writes to the
harness's native session files or implements its lifecycle methods.

`bitrouter acp prompt` runs the **same controller**, in-process: it launches the
harness behind a connection-level controller and drives it over an in-process
duplex channel as that controller's own manager. Session identity is therefore
harness-native there too — there is no `record_id` alias. What `prompt` adds on
top of the controller is client-side: `--turn-timeout` (cooperative
`session/cancel` plus a three-second grace), headless permission denial, OTel
turn spans re-derived from the prompt round-trip, and the NDJSON presentation.

`bitrouter chat` drives the same in-process controller through the same
client, with two additions: it declares a route namespace over the local
daemon socket (so its traffic meters by controller instance, and the
controller decorates `usage_update` with attributed cost), and its `/route`
picker is built on `_bitrouter/route/list|set` — available only when the
initialize metadata advertises them. There is no local engine, `record_id`,
or FIFO turn queue on any path.

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

The same trusted binding advertises `_meta["bitrouter.dev/controller"].usage`
with `version: "1"`, `scope: "session"`, `fields: ["cost"]`, and
`provenance: "bitrouter.dev/cost"`. It means the controller decorates the
harness's own `usage_update` notifications: `used` and `size` are forwarded
untouched, and `cost` is replaced by the spend BitRouter metered for that
native session and its child agents, marked by
`update._meta["bitrouter.dev/cost"] = "router"`. The controller never
synthesizes a usage update — a harness that emits none shows no cost — and
traffic BitRouter did not meter (`--direct`, an explicit `--base-url`, a
harness on its own auth, or a session with no priced requests) leaves the
harness's figure and `_meta` exactly as sent, with no marker. Probe the
capability; an absent or null `usage` means `cost` is whatever the harness
reports.

## Pinned Claude and Codex adapters

The maintained catalog commands are exact pins:

```bash
npx -y @agentclientprotocol/claude-agent-acp@0.75.1
npx -y @agentclientprotocol/codex-acp@1.10.0
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

Native evidence collection runs on `chat`, `acp prompt`, and `acp serve`,
including their `spawn` aliases. It uses the configured BitRouter database and
private invocation spools under `<bitrouter-home>/native-evidence/controllers`.
Codex transcript discovery follows `CODEX_HOME` (default `~/.codex`); Claude
follows `CLAUDE_CONFIG_DIR` (default `~/.claude`). Agent environment overrides
take precedence over inherited values. The collector reads only named sessions
and their explicit dependencies under those roots.

New controllers also recover evidence registered by earlier controllers in the
same BitRouter database and home. Recovery verifies stored profile/spool
registrations, continuously pages through durable records and remaining files,
and can recover hook identities after the originating controller removed the
temporary hook file. Historical Query state is not restored as a live session.
Recovery failures and resource limits remain visible gaps; they do not disable
current collection. No additional user command is needed for this recovery.

A prompt in a confirmed native session automatically starts its first local
task/attempt. Further prompts keep that identity across reconnects. The prompt
record and its task transition commit together; an RPC result starts settlement
but does not certify coding success or completed background work. Outstanding
RPCs owned by another controller remain explicit uncertainty. Task state is
currently available in the application evidence snapshot; TUI feedback and
automatic evaluation submission are not yet wired.

Confirmed prompt boundaries also save local workspace checkpoints. The first
prompt preserves the actual dirty baseline; response checkpoints preserve later
file contents even after further edits or workspace removal. Capture covers Git
tracked files and unignored new files in the repository containing the session
cwd, including shell-produced edits. Runtime directories and the evidence
database are excluded. Additional workspace roots, sparse checkout, non-Git
directories, unsupported file modes and failed or oversized reads remain explicit
coverage gaps. Limits are 8 MiB per file, 16 MiB total raw content, 32 MiB per
serialized artifact and 30 seconds per capture. A prompt response checkpoint is
not the final result of background work or proof that the task passed; final
settlement, evaluation submission and TUI feedback are still pending.

Claude session creation also follows `_meta.claudeCode.options.env`; relative
native roots resolve against that session's `cwd`. Each profile has a separate
evidence namespace and hook spool. Claude may reuse its loaded Query when
load/resume supplies the same cwd and MCP configuration, ignoring new env or
settings. A cached fingerprint cannot prove it survived an idle process exit;
if reuse and recreation would select different profiles, scope stays unknown.
Hooks preserve
the adapter's `CLAUDE_MODEL_CONFIG` fallback when no session settings are given.
Overlapping lifecycle transitions of one session are rejected; close/delete
can still cancel an active prompt. Unknown scope remains an evidence gap until
a successful close or a new controller connection resets it. A failed close
does not prove reset; the adapter may already have removed the Query.

The Codex controller temporarily sets `CODEX_PATH` to BitRouter's private
`app-server` proxy. An existing `CODEX_PATH` is preserved in the child-only
`BITROUTER_CODEX_EVIDENCE_UPSTREAM`; otherwise Node resolves Codex relative to
the maintained adapter package, including nested dependencies. A custom adapter
launcher can supply `BITROUTER_CODEX_ADAPTER_ENTRY` when its package entry is
not discoverable from PATH. `BITROUTER_CODEX_EVIDENCE_SPOOL` is private launch
wiring, not a user-facing model or provider setting.

On Unix, Claude native executables also run through a private stdio proxy.
The controller saves `CLAUDE_CODE_EXECUTABLE` in child-only
`BITROUTER_CLAUDE_EVIDENCE_UPSTREAM` and temporarily points the adapter at
BitRouter. Without an explicit override, resolution follows the adapter's own
SDK dependency and platform-specific native package. Custom adapter launchers
can set `BITROUTER_CLAUDE_ADAPTER_ENTRY` when their entry cannot be located on
PATH. `BITROUTER_CLAUDE_EVIDENCE_SPOOL` and
`BITROUTER_CLAUDE_EVIDENCE_NAMESPACE` are private subprocess wiring. Each
actual process gets its own `cli-<uuid>.jsonl` spool; these UUIDs are distinct
from native conversation ids. Parameters, protocol bytes and native exit codes
are preserved. Script executable overrides and platforms without signal
supervision retain their SDK launch path and an explicit coverage gap.

The Claude proxy captures lifecycle metadata, checks the native profile and
retains process-local order across conversation resets. Registered histories
can recover these records even when the process emitted them before an ACP
session response. That recovery does not restore a live Query or declare that
a task finished. A missing process-stop record remains uncertainty.

Claude collection adds invocation-local lifecycle hooks through the adapter's
session settings, preserving existing hooks. `native-session-hook` and
`app-server` are internal entry points; users do not run them to collect or
rate a session. Neither entry point changes the user's global native config.

Claude prompt hooks retain native prompt IDs when present. Stop observations
retain available background-work metadata; they do not certify that a task has
finished. Child transcript collection follows nested subagent directories and
reads adjacent `agent-*.meta.json` files for explicit parent-agent relations.
Missing parent metadata remains an evidence gap. The derived execution graph retains raw record references and
does not by itself assign task membership or settle an evaluation.

The maintained Claude adapter also requests selected `emitRawSDKMessages`
lifecycle filters while preserving existing filters. The evidence journal keeps
native command states, session idle/running states, task transitions, background
task sets and runtime capabilities as separate observations. Task IDs are not
assumed to be agent transcript IDs. Original notifications still reach the
manager; configuration fields, result text and cumulative cost counters are
excluded from this lifecycle view. Early notifications without a confirmed
session/profile binding remain unbound and do not acquire the default profile's
identity.

Original records survive compaction and context rewind. Fork dependencies use
native ordinal and byte cuts, and later parent work cannot enter the inherited
prefix. Missing history, interrupted lines, unsupported dependencies and
collection failures remain evidence gaps. A live collection snapshot is not a
completed task evaluation or proof that the agent's code passed its tests.

Routing is attempted by default for supported catalog adapters. Use `--direct`
to opt out, `--model` to pin the logical model, `--base-url` to select a daemon,
and `--no-start` to disable local daemon auto-start. Routing/auth failures occur
before the ACP handshake.

When API authentication is enabled, local routing uses the normal BitRouter
API/virtual key for both model requests and the route principal. Under
`skip_auth: true`, both use the deliberately shared `local` principal. The
owner-only daemon socket carries route mutations but does not mint or validate
a second route namespace. An explicit remote `--base-url` can still
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
labels. The controller decorates, and never synthesizes, manager-facing
per-session cost; see the `usage` capability above.

## One-shot NDJSON

`acp prompt`/`spawn -p` emits a first `session` line carrying the
**harness-native** `session_id` (plus `agent_session_id` when the harness
exposes one), `agent`, `via`, and `launch_id`. `launch_id` is the one that
joins to spend: the daemon attributes ACP traffic by an authenticated
controller namespace, which only `acp serve` and `chat` declare, so a prompt session's
rows carry no controller instance to key on.
It no longer carries `record_id`; that alias is off the wire. Then come
`message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, and `usage`
lines, a `permission` line for each request the headless policy answered
(`--deny-all` by default; `--approve-reads`, `--approve-all`, or a per-tool
`--permission-policy`; exit 5 when something was denied and nothing approved),
and a `result` line. `--no-wait` emits `submitted`. This NDJSON presentation is
`--format json`, the default; `--format text` and `quiet` print the transcript
or the assistant text instead. It belongs to `prompt` only; it is not the
`acp serve` wire format.
