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
BitRouter does not generate an alias or read Claude/Codex private session
files. Optional local recording mirrors observable ACP content; it does not
replace the harness's native session catalog or persistence.

`bitrouter run` runs the **same controller**, in-process: it launches the
harness behind a connection-level controller and drives it over an in-process
duplex channel as that controller's own ACP client. Session identity is therefore
harness-native there too — there is no `record_id` alias. What `prompt` adds on
top of the controller is client-side: `--turn-timeout` (cooperative
`session/cancel` plus a three-second grace), headless permission denial, OTel
turn spans re-derived from the prompt round-trip, and the NDJSON presentation.

`bitrouter code <agent>` drives the same in-process controller through the same
client, with two additions: it declares a route namespace over the local
daemon socket (so its traffic meters by controller instance, and the
controller decorates `usage_update` with attributed cost), and its `/route`
picker is built on `_bitrouter/route/list|set` — available only when the
initialize metadata advertises them. There is no local engine, `record_id`,
or controller-owned FIFO turn queue. Code keeps an explicit process-local
follow-up queue that dispatches only after normal turn completion; abnormal
stops pause queued work for explicit action.

## Controller launch and initialization

```bash
# ACP-client-driven, multiple native sessions on one harness connection
bitrouter acp serve <id> [--config PATH]

# One-shot client over the same controller
bitrouter run <id> "prompt" [routing flags]
```

Stdout is ACP JSON-RPC and logs go to stderr. The ACP client sends `initialize`
first. BitRouter forwards the client's capabilities and `_meta` to the
harness, initializes the harness exactly once, configures its BitRouter model
endpoint when supported, then returns initialize success. Client-facing
`agentInfo` identifies `bitrouter-acp-controller`; sanitized harness and pinned
adapter identity are under `_meta["bitrouter.dev/controller"]`.

The controller passes through harness lifecycle capabilities, but removes the
internal custom-provider capability. Standard `providers/*` configures the
harness endpoint from controller to harness; it is not a client-side
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
use client-side `providers/*` as a compatibility alias.

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
labels. The controller decorates, and never synthesizes, client-facing
per-session cost; see the `usage` capability above.

## One-shot NDJSON

`run` emits a first
`session` line carrying the
**harness-native** `session_id` (plus `agent_session_id` when the harness
exposes one), `agent`, `via`, and `launch_id`. `launch_id` is the one that
joins to spend: the daemon attributes ACP traffic by an authenticated
controller namespace, which only `acp serve` and `code <agent>` declare, so a
prompt session's rows carry no controller instance to key on.
It no longer carries `record_id`; that alias is off the wire. Then come
`message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, and `usage`
lines, a `permission` line for each request the headless policy answered
(`--deny-all` by default; `--approve-reads`, `--approve-all`, or a per-tool
`--permission-policy`; exit 5 when something was denied and nothing approved),
and a `result` line. `--no-wait` emits `submitted`. This NDJSON presentation is
`--format ndjson`, the default (`json` remains an alias); `--format text` and `quiet` print the transcript
or the assistant text instead. It belongs to `prompt` only; it is not the
`acp serve` wire format.

## Local ACP recording

Recording is opt-in and independent of the content-free `trajectory.enabled`
route ledger. In `bitrouter.yaml`:

```yaml
acp_recording:
  enabled: true
```

The shared controller records observed prompts, session updates, tool inputs
and outputs, permission/terminal/filesystem callbacks, responses, and lifecycle
facts before forwarding them. It applies to `code`, `run`, and `acp serve`,
including direct sessions. Content stays in the configured local database;
this setting does not invoke a judge or publish transcripts. Initialization,
authentication, provider configuration, and MCP launch credentials are excluded.
Recorded user/tool content can itself contain sensitive information.

```bash
bitrouter acp recordings list --agent codex-acp
bitrouter acp recordings show --agent codex-acp NATIVE_SESSION_ID
bitrouter acp recordings delete --agent codex-acp NATIVE_SESSION_ID
```

Use the resolved configured agent ID as the source namespace. Add `--config`
before `list`, `show`, or `delete` to select another configuration. JSON is the
default; `--human` renders a readable timeline. Data is retained until explicitly
deleted. Deletion removes local content and fences subsequent writes for that
recorded identity; native harness history and model metering remain intact.
Disable recording to continue using a deleted native session without recording.

Load replay is retained separately from live canonical events. It is never a
new model execution or additional cost. A gap is reported when history across
load/resume cannot be verified; equal text is never sufficient to deduplicate
legitimate repeated prompts. Interrupted/unclosed captures remain explicit.
With recording enabled, a durable-write failure stops forwarding with an error;
it must not silently produce an apparently complete record.

Request links use the locally established controller/principal namespace and
observed native IDs. They preserve known model/provider, route-ledger evidence,
and charge provenance. Tool-to-request relationships that ACP does not expose
remain unresolved. Direct/remote or unmetered requests cannot be claimed as
complete local cost; observed costs are summed over unique request IDs.

### Checkpoints and imported assessments

Read the native session's `head` from `acp recordings show`, then freeze that
exact prefix. The agent source is the configured ID used when recording.

```sh
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH create --watermark N
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH list
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH show CHECKPOINT_ID
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH resources CHECKPOINT_ID --refresh
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH submit assessment.json
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH history
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH effective
bitrouter acp checkpoints --agent SOURCE NATIVE_ID --config PATH family
```

Creation rejects an outdated watermark. Existing checkpoints keep original tool
versions when a session appends. Resource refresh reads existing local records
only; omit `--refresh` to inspect observation history. No judge, harness, test,
PR query, or routing publication is started by these commands.

An assessment JSON object requires `submission_id`, `checkpoint_id`,
`expected_revision` (null only for the first selection), `source` (`human` or
`agentic`), `evaluator_id`, `evaluator_version`, `reason`, and `assessment`.
The assessment contains SHA-256 `pipeline_config_digest` and `selection_digest`,
`scores`, `evidence`, and `explanation`. Scores map criterion IDs to
`{"status":"scored","value_ppm":500000}`, `{"status":"unknown"}`, or
`{"status":"not_applicable"}`. Evidence entries identify a checkpoint's
`node_id` and `digest`. This store validates references and ranges, not rubric
applicability or semantic correctness.

For a correction, read `effective`, use its `current_revision`, and supply a new
submission ID. Identical retries are idempotent. A stale expected revision is
rejected; an automatic result cannot displace a Human correction on the same
checkpoint. `assessment: null` explicitly retracts the current assessment and
requires Human source and a reason. Old results do not automatically revive.

An append marks the previous label stale. Fork views expose related labels as
one family and union request costs rather than adding checkpoint totals. Deleting
recordings also invalidates referencing checkpoints and removes their assessment
text, including inherited references in descendant checkpoints.
