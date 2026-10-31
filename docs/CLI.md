# BitRouter CLI Reference

`bro connect [codex|claude] --model ID [--display-name LABEL]` adds a gateway model to native model
pickers (default: `bitrouter/automatos`). Omit the target for an interactive menu; use `--dry-run` to preview,
`--yes` for scripted setup, and `--remove` to restore owned settings (Codex and Claude Desktop preserve unrelated settings).
Works without `harness` or `core`. `--base-url URL` selects an explicit gateway
without local config/startup. Read the [connection contract](../skills/bitrouter/references/connect.md)
for gateway auth, client differences, restart requirements and recovery.

MCP proxy configuration and `mcp check`/legacy `tools` diagnostics require `mcp`.
Without `harness`, the daemon opens no local task or native ACP listener, and
`agent_api.enabled` and `acp serve` report the missing capability before startup.
Empty MCP configuration and disabled agent API settings remain readable.
Local native headless execution requires both `core` and `harness`.
Supervised sessions and ACP route-lease control also require `harness`; a
gateway-only daemon leaves the supervisor ledger untouched. Request identity
and existing session-attributed spend remain available.
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
`bro shared init --config PROCESS.yaml --plan SEED.json --operator-id OPERATOR
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
`bro shared apply --endpoint URL --token-env ENV --input MUTATION.json` submits
one literal JSON mutation. `--input -` reads stdin; input is limited to 2 MiB
and contains its caller-owned `operation_id`. No configuration or environment
substitution occurs inside the input. Endpoint and token are checked before
opening it. All remote shared commands require `multi-instance` and reject
named contexts before discovery or input reads.

`bro shared operation --endpoint URL --token-env ENV --operation-id ID` reads
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

Evidence and learning commands require the application's `core` feature:
`eval`, `optimize`, `workflow-state`, and `policy compile`, `publish`, `verify`
and `evolve`, plus `acp evolution` and checkpoint `judge`, `judge-job` and
`rubric` operations. Without core they report the requirement before opening
inputs or stores. Nonempty `BITROUTER_WORKFLOW_TRACE_JSONL` also requires core.
`serve`, `start` and `restart` reject it before creating runtime files or stopping
a resident daemon. Published-policy loading/enforcement, ordinary
policy inspection/reload/rollback, request metering and trajectory inspection,
replay and pruning remain available without the learning runtime.

`bro <subcommand> [flags]`

## Executable identity

`bro build-info` reports the local executable's version, source revision (or
`null` when unavailable), target platform, sorted effective features, distribution
profile and handoff fingerprint. It reads no configuration or service storage.
Use `--human` for a text view. The profile is `gateway` for no optional features,
`full` for the manifest's default capability set, and `custom` for other sets.
Aliases and transitive features are normalized at build time; `--all-features`
is a custom selection, not the official full profile.

## Output format

Every command prints a single **formatted JSON object** to **stdout** — success or failure — so output is machine-parseable by default (agent-native first). Global flags:

- `-j`, `--json` — force JSON (the default).
- `--human` — render a human-readable view to stdout instead of JSON.
- `--context <name>` — run a supported administration action against a named remote
  BitRouter target; `local` explicitly selects normal local behavior for commands
  that support it. `tenancy` HTTP commands require a named remote target.
- `-H` before the subcommand (for example, `bro -H cloud whoami`) — compatibility spelling for `--human`. Under `bro cloud api`, `-H` means `--header`, matching `gh api`.
- `-h`, `--help` — unchanged (`-h` is **not** human output).

All diagnostics — progress, warnings, internal logs, and a human echo of errors — go to **stderr** (colored when stderr is a TTY; honors `NO_COLOR`). So:

```
bro <cmd> 2>/dev/null | jq .
```

always yields one clean JSON value. A failed command emits a uniform error envelope to stdout and exits non-zero:

```json
{ "error": { "kind": "not_found", "message": "…", "context": ["…"], "hint": "…" } }
```

`kind` is a stable taxonomy (`bad_request` / `unauthorized` / `forbidden` / `not_found` / `upstream` / `internal` / …). Under `--human`, the result (success object or error block) is rendered to stdout in the human form and no JSON is printed.

> Non-reporting commands are exempt: `serve` is a long-running server; `acp serve` is a stdio JSON-RPC bridge; foreground `run`, `task run`, `task managed`, and management `events --follow` emit NDJSON; `launch` inherits the terminal for the external harness; and `cloud api` streams the remote response body. Bare `agents`, `run --background`, and single-operation management commands remain ordinary structured reports.

## Native tasks through the managed core

`bro task managed PROMPT [--model PROVIDER/MODEL] --workspace PATH --session NAME`
is a client of the selected local daemon's managed v1 endpoint. The daemon owns
NativeSession/CoreSession, native workspace tools, resource connections and the
original bro_managed_sessions checkpoint/ACK row. The CLI does not assemble App,
open the database or run a second model loop. A missing daemon is started through
the existing readiness path; an older daemon without managed v1 is rejected.
The daemon admits at most four active managed native sessions; excess requests
fail before native resource/owner admission. This limit is declared by v1 Hello.

It accepts `--config`, `--effort`, `--read-only`, `--check`,
`--max-output-tokens`, `--request-id` and `--server-instance`. An omitted model
uses the running daemon's startup defaults; saved config edits do not silently
rebind that daemon. Workspace is captured from the client's directory before
startup. Read-only conflicts with verification; coding preserves its original
headless approval policy. Remote contexts remain unsupported.
`--check` retains native tool-outcome semantics: a normally exited shell is
currently recorded as executed successfully even with a nonzero `exit_status`.
The original status/output stays in the checkpoint; nonzero alone does not
currently fail native verification. Rejected or unexecuted verification does.

Output remains NDJSON (`managed_session`, then `terminal`) with the original
fields plus request, daemon and native ownership identities. Stderr prints the
session/request/daemon key before submit; that line is intent, not an acceptance
receipt. Nonzero exit reports failure, cancellation, unreleased state or an
unavailable historical terminal result. A completed run alone does not prove
that release succeeded.

Reuse the same session, input and `--request-id` after an unknown ACK. The CLI
only queries the original operation when its submit reply is lost; it does not
resubmit implicitly or renegotiate a new boot. Durable duplicate keys return
the original acceptance without model/tool replay. Changing input under that
key fails. A new input key can continue a confirmed released session in the
same workspace/mode, retaining session/harness IDs and increasing its epoch.

`bro task managed-status --session NAME [--request-id KEY] [--config PATH]`
reads the live owner's durable state or its retained journal without loading
resources, starting the daemon, restoring a session or calling a provider.
`--socket` selects the owner-local control endpoint; `--server-instance` can
pin the expected daemon. A prior accepted run may be historical while its
receipt remains available; this is reported separately from its current run.

`bro task managed-cancel --session NAME --request-id KEY --server-instance BOOT`
requests cancellation only for that exact active input. Its receipt does not
prove settlement: query status until the owner is no longer resident and release
is confirmed. Ctrl-C in the submitting CLI makes the same cancellation request
and waits for settlement. Killing/disconnecting an observer does not cancel the
daemon's task. Daemon shutdown cancels and joins native drivers and tools.
Settled processes can still have unknown effects: in that case status remains
recovery-required and unreleased rather than claiming a successful release.

Abrupt daemon loss, unknown effects and unconfirmed release remain
recovery-blocked; no startup/query/request automatically retires the old owner
or replays accepted work. The existing database/journal format is retained.
Library users may still embed NativeSession directly. See the managed-core
reference for subscription output reservations and storage bounds.

MCP skills require explicit per-skill consent, including local commands their
instructions may induce; ordinary headless command policy grants no consent.
`managed` emits a `managed_skill_consent` record and waits. Inspect the full
source/entry with `managed-status`, then use
`bro task managed-answer --session NAME --request-id KEY --server-instance BOOT --consent-id ID --decision allow|deny`.
`--config` or `--socket` selects the same owner-local endpoint. The consent ID
binds the original agent Turn, owner epoch, complete entry and permission state;
answers use a stable operation key. Repeating an identical committed answer
returns its original `answer_receipt`, including after daemon restart, without
reopening a native owner or reading MCP files. Cold state accepts no new consent.
`task run` likewise waits on `mcp_skill.activate`; use native `threads pending`
and `threads answer` to answer it. Skill approval never expands tool permissions.
Verified MCP instructions enter the model as source-tagged user data, with the
complete held entry; they are never inserted into system instructions or
automatically shared into an unconsenting child task. Original MCP root/file caches remain in the Turn and are reverified before reuse.
Complete budget and daemon/recovery qualification remains G09 work.

## BRO native Threads and tasks

```console
bro task run "fix the failing test" --check "cargo test"
bro task run "inspect this project" --read-only
bro threads create --workspace /path/to/project --model bitrouter/auto
bro threads show THREAD_ID
```

`task run` starts or connects to the local `bro serve` process through its
owner-restricted native socket. The client creates a Thread, starts one Turn,
and waits for that Turn's result. Each call uses its own stable durable
acceptance key. A lost reply is retried once with the original key and server
instance; an unresolved outcome reports its identity for inspection.
A known start rejection reports the created Thread ID; creating a Thread alone
never executes a model or tool. There is no independent Task execution API.

`task run` and `task managed` accept repeatable `--skill SELECTOR` and
`--exclude-skill SELECTOR`. Select exact identities with `project:DIRECTORY_ID`,
`global:DIRECTORY_ID`, or `mcp:SERVER:URI`, for example
`--skill mcp:reviews:skill://review/SKILL.md`. Inclusion is required; exclusion
prevents selection. Contradictory selectors fail before submit. MCP URIs missing
from listing use metadata-only direct lookup under the same caller/workspace
source policy, then require explicit content consent. These flags never install
skills or grant tools. Native Thread constraints persist with its configuration;
managed constraints persist in the original input and its retry fingerprint.
Clients require the daemon's `skill_constraints` capability, so older daemons
cannot silently ignore the selectors. Missing required entries fail the accepted
work before model execution; directory list/show/read/write remain separate.

Selectors preserve generic URI/IRI identity, including rootless forms such as
`--skill 'mcp:reviews:urn:review/SKILL.md?revision=1#instructions'`. Query and
fragment stay in the original entry and approval; the path determines the skill
directory. Relative support-file paths resolve only inside the held manifest;
ambiguous encodings or versions require an exact URI. No authority is resolved
from a skill URI, and the scheme does not grant local filesystem access.

Native models can request `skill_read {skill_id, reference}` for a currently
selected, approved parent. The path must occur in its original receipt/held
manifest. The tool returns a metadata receipt; verified UTF-8 content enters the
same Turn as source-tagged user data and stays out of ordinary tool history.
Reads do not execute scripts, grant tools or activate nested skills. Original
frozen file/context budgets apply; content is rejected rather than truncated.
This state requires `native_skill_files_v1`; legacy Turns receive no new tool.

Standard output is NDJSON: `accepted` includes `thread_id` and `turn_id`,
`snapshot` carries a Thread view, durable `event` records use Thread cursors,
`live` carries bounded volatile output, and `terminal` includes status,
verification, final answer, and cursor. Failed, cancelled, interrupted, or
recovery-blocked Turns exit nonzero. This local headless client approves its
own identified tool requests. `--check` supplies bounded workspace verification;
without it verification is `not_requested`. `--read-only` forbids effectful
tools and cannot be combined with `--check`. Explicit remote contexts fail
without local fallback. `bro run <agent>` remains the separate ACP harness path.

Unchanged routing observations and events without public changes are omitted
from stdout. Cursors remain monotonic but need not be consecutive.

BRO coding Turns expose six tools: `read(path, offset?, limit?)`,
`glob(pattern, path?, limit?)`, `grep(pattern, path?, glob?, ignoreCase?, literal?, limit?)`,
`write(path, content)`, `edit(path, edits[{oldText, newText}])`, and
`shell(command, timeout?)`. Read-only Turns expose only `read`, `glob`, and `grep`.
Unavailable tools and legacy `ls`, `find`, `bash`, and `powershell` names are
rejected before approval or execution. File paths are relative to the workspace.

`read` accepts UTF-8 files up to 2 MiB or one directory, including `path: "."`.
One-based offsets count file lines or directory entries. Directory listings
include hidden/ignored children and identify symlinks; complete pages fit in
50 KiB and return the next offset when truncated. `glob` and `grep` respect
project ignore rules, skip child symlinks, and return workspace-relative paths.
Defaults are 500 directory entries, 1,000 glob results, and 100 grep matches;
limits may be raised to 2,000. `write` creates parent directories and overwrites
files. `edit` requires unique, nonoverlapping spans in the original file and
preserves untouched text, BOM and line-ending style.

Coding execution selects Bash then sh on Unix, or pwsh then powershell.exe on
Windows, before model sampling. The declaration and shell result identify the
selected executable/dialect; launch failure never retries another interpreter.
Verification shares the Turn's selected executor. No supported interpreter
fails coding admission; read-only execution needs none. Shell commands start in
the workspace, default to 30 seconds, and allow at most 120 seconds. Workspace
path checks are not an OS sandbox for commands. Read workers remain bounded;
write, edit, shell and verification retain exclusive execution and durable
intent/result commit boundaries. Historical names/IDs/results remain unchanged;
unsettled legacy work never executes through aliases or reuses old approvals.

The local native protocol is **v19**, bound to the negotiated server instance.
Older clients/daemons fail the handshake before submission or approval. The v19
boundary prevents v18 task clients from auto-allowing MCP skill requests. HTTP
v2 capabilities advertise `explicit_skill_consent: true`; its `/inputs` body
requires `skill_consent: true` for `mcp_skill.activate`, alongside the exact
Turn/request and decision. Ordinary command policy cannot supply that consent.
`command_id` correlates
transport replies; durable `idempotency_key` identifies accepted operations.
Every Thread operation checks its authenticated caller, stored permission
profile, current workspace grant, and instance. Local and HTTP callers have
distinct ownership. Knowing a Thread ID does not authorize access.

`bro threads` sends single operations to the same ThreadService. `start` begins
a Turn, `enqueue` adds FIFO input, `steer --turn` targets active work, and
`pending/answer` uses the exact Turn and request identity. `cancel` distinguishes
active and queued Turns; `resume` only resumes a paused queue. `close` joins work
and retains public history; `load` does not execute or resume.

`list/show/history/events` inspect authorized state. Cold reads do not install
model context or workers. A directory page scans at most 16 roots; its cutoff
fixes membership. Event observation appends text or NDJSON. Disconnecting an
observer never cancels accepted work or answers approval. Recovery blockers and
unknown effects remain inspectable and cannot be bypassed by resubmission.

Native defaults retain `chat.model`, configured prompt templates, model-policy
routing, output reservations and existing permission profiles. There is no
first-party composer, menu or persistent conversation input loop.
See [the control reference](#single-operation-native-and-acp-controls) for
flags, exact retries, history limits and explicit recovery.

The same Thread service can expose an optional HTTP listener:

```yaml
agent_api:
  enabled: true
  listen: 127.0.0.1:4359
  token_env: BRO_AGENT_API_TOKEN
  workspaces: [/absolute/server/project]
```

The token environment value must contain at least 24 bytes. The listener binds
only to loopback and authorizes exact canonical workspaces. Every `/agent/v2`
request requires `Authorization: Bearer <token>`. Read `/agent/v2/capabilities`
first and send its `runtime.server_instance_id` as `X-Bro-Server-Instance` on
all other operations. Every mutation requires `Idempotency-Key`.

| Route under `/agent/v2` | Body or result |
| --- | --- |
| `POST /threads` | `workspace`, `model`, optional `effort`, `read_only`, `verification_command`; returns Thread |
| `GET /threads/{id}` | Thread view, including latest Turn and queue |
| `POST /threads/{id}/turns` | `prompt`, `mode: "start"` or `"enqueue"`; returns receipt |
| `GET /threads/{id}/turns/{turn_id}` | Targeted stored Turn snapshot |
| `POST /threads/{id}/turns/{turn_id}/cancel` | `mode: "active"` or `"queued"`; explicit scope remains stable on retry |
| `POST /threads/{id}/steer` | `expected_turn_id`, `text` |
| `POST /threads/{id}/inputs` | `turn_id`, `request_id`, `approved` |
| `POST /threads/{id}/resume` | Explicit queue resumption; no body |
| `GET /threads/{id}/history?after=N&cutoff=C&limit=L` | Bounded durable public history; cutoff and limit optional |
| `GET /threads/{id}/observe?after=N` | SSE Thread snapshot/catchup, durable events and volatile live output |

The old `/agent/v1/tasks` routes are removed. The listener is disabled by
default and does not inherit inference `server.skip_auth` or read-only control
credentials. Slow subscribers or unavailable hot cursors get a fresh snapshot
with `resynchronized: true`. Catchup precedes the snapshot cutoff; do not apply
it again to that snapshot. Live deltas do not advance durable cursors or enter
settled model context. Complete assistant/tool results are projected from the
same transaction that saves their canonical execution facts.

The database retains Thread identities, context, history and acceptance keys.
Idle or safely settled empty paused Threads can leave the hot cache. Pressure
reclaims the oldest eligible hot Thread; active workers, queues, approvals,
subscribers, commit references, recovery blockers and workspace leases prevent
unload. Cold public reads do not install SDK context or run work. Clean
same-instance reload preserves cursor/configuration/context without journal
writes. Cross-instance inspection remains separate from explicit safe recovery;
loading or reconnecting never resumes a stored queue. Use explicit `bro threads recover --from` for the original recovery request;
queue resumption is a separate operation.

Defaults: 8 active Turns, 32 hot Threads, 32 queued inputs per Thread, 2 MiB
context per Thread / 64 MiB hot context, and 32 settled Turn snapshots / 64 MiB
for up to 30 minutes. Thread observation caches retain 256 events / 2 MiB,
with 8 subscribers and 32 queued events per subscriber. Live snapshots retain
at most 32 KiB. Evicting a snapshot or hot Thread does not remove durable keys
or history. Local connections and HTTP handlers each admit at most 64 requests.
All-ineligible capacity returns `overloaded`.

Read batches use at most 4 workers per Turn and 16 globally. Write/edit/shell
and verification preserve ordered workspace barriers. Requests, full responses,
execution intents/results and settlement commit before dependent advancement.
Verification asks for its own identified approval; denial reports `denied`.
Cancellation joins started work before clean release. Storage failures and
unknown effects retain workspace exclusion as `recovery_required`.

Startup claims one database owner and completes bounded cold discovery before
admission: 1024 roots, 1,000,000 scanned records and 4 MiB cold metadata.
Recovery reads allow 2 readers, 64 records / 4 MiB per page and 1,000,000
records per Thread. Capabilities expose aggregate progress, not root identities
or prompts. New roots use runtime format 2. Migration adds metadata with
**default 0** for old roots; unsupported formats fail before payload decoding
or execution, with `recovery_required` / `unsupported_runtime_format`.
They are not converted or marked safe.

Normal shutdown joins execution and commits release/stopped evidence. An active
lost owner, unsupported root, or unconfirmed effect remains blocked; PID loss
and a new instance do not prove safe termination. Workspace coordination uses
stable `.bro-workspace-<sha256(canonical UTF-8 path)>.lock` / `.json` sidecars
in the writable parent, including across different databases. An unreleased
marker remains blocked after process loss. Never delete owner records, database
roots or markers to bypass this state. This cooperating runtime exclusion is
not an OS sandbox or a power-loss guarantee.

Per-provider credential commands are under `bro providers (login|logout)`;
BitRouter Cloud sign-in is `bro cloud (login|logout|whoami)`.

## Single-operation native and ACP controls

These commands call the running daemon's existing owners. They never enter a
chat loop or start a daemon/agent as a side effect of a query. Native Thread IDs
and supervised ACP run IDs are separate namespaces. All accept `--config PATH`,
`--socket CONTROL_SOCKET` (overrides config discovery), and an optional
`--server-instance` pin. Native commands pin the **native runtime instance**;
ACP/evolution commands pin the **control daemon boot ID**.

| Native command | Operation |
| --- | --- |
| `threads list [--after N --cutoff N --limit 1..16]` | Bounded cold directory |
| `threads create --workspace PATH --model SELECTOR` | Create Ask Thread, no Turn; accepts `--skill`/`--exclude-skill`, `--effort`, `--model-policy`, `--max-output-tokens`, and `--read-only` or `--check COMMAND` |
| `threads show THREAD [--turn TURN]` | Inspect exact native identity and recovery blockers |
| `threads history THREAD [--after N --cutoff N --limit 1..1000]` | Durable history page |
| `threads events THREAD [--after CURSOR] [--follow]` | Initial bounded snapshot/catchup; optional NDJSON follow |
| `threads start THREAD --prompt-file PATH` | Start one Turn on idle Thread |
| `threads enqueue THREAD --prompt-file PATH` | Durable FIFO acceptance |
| `threads steer THREAD --turn TURN --prompt-file PATH` | Steer exactly the named active Turn |
| `threads pending THREAD` | Current actual Turn/input identity |
| `threads answer THREAD --turn TURN --request INPUT_ID allow/deny` | Answer one original approval |
| `threads cancel THREAD --turn TURN [--queued]` | Cancel active or withdraw exact queued Turn |
| `threads resume THREAD` | Resume paused queue; never execution recovery |
| `threads close THREAD` | Withdraw queue, cancel/join worker, retain reusable paused Thread/history |
| `threads unload THREAD` | Unload safe resident state |
| `threads load THREAD --workspace PATH` | Explicit OS-local grant reconstruction/load; no continuation, no widening existing profiles |
| `threads recover THREAD --from JSON_FILE` | Exact `ThreadRecoveryRequest`: `source_server_instance_id`, `source_cursor`, `idempotency_key` |

Create/start/enqueue/steer/answer/cancel/resume/close accept `--request-id KEY`
(1–128 bytes, no controls). After an unknown ACK, retain the original input,
request key, target and instance; never change identity to repeat an effect.
`--prompt-file -` reads stdin once. Create defaults to Ask; effectful tools and
verification keep their independent approvals. Read-only conflicts with checks.
Reads reconstruct no grant and start no worker. Unknown effects or unsafe source
owners continue to block recovery. A successful load is no evidence of recovery.

Native single-operation reports contain `kind`, `server_instance_id`,
`request_id` and a typed `payload`; `--human` shows the same facts as plain text.
History cutoffs/cursors, resynchronization and volatile event deltas stay
distinct. Ctrl-C/disconnect during follow only stops observation.

| Supervised ACP command | Operation |
| --- | --- |
| `agents show RUN` / `pending RUN` | Read snapshot / exact permission IDs and offered options |
| `agents events RUN [--after SEQ] [--follow]` | Read retained replay or append NDJSON |
| `agents prompt RUN --prompt-file PATH` | Submit one prompt and return ACK |
| `agents answer RUN --request PERMISSION_ID --option OPTION_ID` | Answer exact offered option |
| `agents cancel RUN` / `review RUN` | Cancel turn / mark result reviewed |
| `agents mode show RUN` / `mode set RUN MODE` | Agent-reported modes and confirmed mutation |
| `agents config show RUN` / `config set RUN CONFIG_ID --value-json JSON` | Reported options; JSON string, boolean or ACP typed value object |
| `agents route show RUN` / `route set RUN ROUTE` / `route reset RUN` | Actual controller/session route; no global preview substitution |
| `agents session list RUN [--cwd PATH --cursor CURSOR]` | Agent's native-session directory |
| `agents session new RUN` / `load RUN SESSION` / `resume RUN SESSION` | Capability-gated native lifecycle on existing controller |

Reads acquire no control lease. Mutations use exact-scope grants and transient
leases without takeover; they accept `--request-id` and `--client-id`. Reports
retain client, generation, action and boot IDs. An exact unknown-ACK retry also
requires original `--lease-generation` and `--server-instance` and acquires no
new lease. `agents action RUN` with all four identity flags reads the original
ACK without applying its action. Supervisor protocol v2 added this inspection;
v3 adds scoped skill constraints on background starts. Existing v1/v2 operations
remain accepted, but skill constraints require v3 and are rejected before effects
on older requests. This is separate from ACP wire v1/v2. The cache is boot-local; a missing ACK
is unknown, never permission to resubmit. Unsupported/failed settings and route
operations error without optimistic success. Direct/custom agents without a
route bridge and explicit remote base URLs cannot claim session routing.
The CLI confirms the discovered boot again after receiving its boot-local
grant. It never refreshes that grant across a restart. A boot change during
authorization fails before any session operation; a later restart invalidates
the cached authorization. New clients require the v2 supervisor and do not
downgrade and resubmit to a legacy daemon.

Events report `first_retained_seq`, `snapshot_seq`, `history_complete` and a gap
indicator; future cursors fail. Retained replay is not complete durable history.
Existing `agents stop` / `remove` remain separate process/metadata operations;
removal does not delete harness-owned native sessions. Foreground `run` still
denies unmatched permissions (exit 5 if all decisions were denials); background
Ask includes permission wait in its timeout. Raw ACP auth/fork/extensions remain
available through their existing protocol capabilities.

`bro acp evolution` retains its existing publication/restore operations and adds
`candidate catalog`, `candidate revision BLOCK`, `candidate preview --from FILE`,
`candidate register --from FILE`, and `judge-model MODEL` (no mode change).
Preview consumes a `CandidateSpec`; register consumes the exact reviewed
`CandidatePreview` with digest/generation fences and never silently rebases it.
`checkpoint --source SOURCE --session NATIVE_SESSION` offers `list`,
`freeze --expected-watermark N`, `review CHECKPOINT`, and `submit --from FILE`.
Review returns frozen evidence, revision/history and optional prefill; reading
it changes no reward. Submit requires an exact `RubricSubmission`, including
explicit `expected_revision` even when null. These operate through the live
owner with the shared endpoint flags above; they retain exact revision checks,
evidence requirements and operator reasons.
Local evolution control negotiates v1 through `EvolutionHello`, then sends
`EvolutionFenced` with that version and control boot. The daemon checks both
before invoking the owner, including when no explicit `--server-instance` was
supplied. Legacy/unknown contracts fail before mutation; the new CLI never
falls back to the original unpinned `Evolution` operation. Existing clients
using that legacy operation retain their original behavior.

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
- **Turn on `bitrouter::observe::span_attributes` when a forwarded attribute does not appear on a span.** A deployment can attach its own attributes to the root `chat` span, but the span schema reserves its own vocabulary: keys under `bitrouter.` or `gen_ai.`, and any key the schema already declares (`$screen_name`, `error.type`, `server.address`, …), are **dropped rather than stamped**, so one deployment cannot redefine what an attribute means for everyone else. The drop is deliberate and per-request, hence DEBUG rather than WARN: `RUST_LOG=info,bitrouter::observe::span_attributes=debug` names each dropped key. The full reserved region is `crates/bitrouter-sdk/span-schema.json`.
- **`RUST_LOG` no longer affects tracing.** This used to be the opposite, and the reversal is worth stating because the old advice is still in circulation: the ingress span was a `tracing` span bridged into OpenTelemetry, so a filter that dropped `bitrouter::observe::http` at INFO also dropped the SERVER span, and every `chat` span exported as an orphan root with no error reported anywhere. A blanket `RUST_LOG=warn` was enough to do it. The ingress span is now an OpenTelemetry span in its own right and never passes through the `tracing` subscriber, so **no filter can suppress it**. Set `RUST_LOG` for the logs you want; traces are unaffected either way.

## Ignored configuration

`Config::plugins` is an unvalidated map and the JSON Schema declares it
`additionalProperties: true`, so a `plugins.<id>` block the binary does not
read is **silently ignored** — a typo like `plugins.bitrouter-guardrail`
(singular) drops the operator's declared block / redact patterns and the
process starts anyway. Two places report it:

- `bro config validate` lists them under `ignored_config`. It does not
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
bro context add workstation \
  --endpoint https://router.example/control/v1 \
  --token-env WORKSTATION_BITROUTER_TOKEN
bro context list
bro context show workstation
bro --context workstation status
bro --context workstation requests
bro --context workstation models --provider openai
bro --context workstation route openai/gpt-5
bro --context workstation providers list
bro --context workstation observe status
bro --context workstation policy status       # active by default
bro --context workstation policy show default --view disk
bro --context workstation agents list
bro --context workstation reload
bro context remove workstation
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
A remote policy command defaults to `--view active`; local policy reads default
to `--view disk`.

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

The HTTP subcommands of `bro tenancy` are available in the base gateway build.
They are client adapters;
the server must implement the selected tenancy operation. Choose either a named
context or an explicit `--endpoint URL --token-env NAME` pair on each remote
command. Mixing them is an error. There is no default target, local daemon
discovery, database access, or `--context local` fallback. The token comes only
from the selected environment variable; no token-value flag is provided. HTTPS
or loopback HTTP is required. The client follows no redirects and preserves the
existing CLI system-proxy behavior.

```console
bro context add resource-admin \
  --endpoint https://router.example/control/v1 --token-env RESOURCE_ADMIN_TOKEN
bro --context resource-admin tenancy capabilities
bro tenancy capabilities --endpoint http://127.0.0.1:4358 --token-env RESOURCE_ADMIN_TOKEN
bro tenancy configuration validate --input draft.json
bro --context resource-admin tenancy configuration apply --tenant tenant-a --input draft.json
bro --context resource-admin tenancy configuration read \
  --installation-id install-a --tenant tenant-a --revision config-r1
bro --context resource-admin tenancy configuration active \
  --installation-id install-a --tenant tenant-a
bro --context resource-admin tenancy configuration activate --tenant tenant-a --input activation.json
bro --context resource-admin tenancy operation --installation-id install-a --operation-id prepare-1
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

### `bro serve`

Run the HTTP server and control socket **in the foreground**.

```
bro serve [-c <path>]
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

The manifest's `format: 1`, `installation_id`, and `resources` identify an
explicit installation catalog. Each resource contains its qualified `reference`
and a route, policy, or credential `definition`. Routes retain their complete
routing source, selector and explicit provider/account credential references;
policies retain a policy-lock document and named entry. The initial importer
accepts static API-key credentials with generation zero; OAuth/ambient stores
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

### `bro start`

Spawn `serve` as a **detached background daemon**.

```
bro start [-c <path>] [--log <path>]
```

Logs default to `bitrouter.log` next to the config file (e.g. `~/.bitrouter/bitrouter.log` when the config resolved to `~/.bitrouter/bitrouter.yaml`). Refuses to start if a daemon is already running.

Waits until the daemon answers on its control socket before reporting `✓ … started` (up to 15s), then prints the listen address and routable-model count — so a follow-up command can rely on the daemon being up. If the daemon crashes during startup, the tail of its log is printed and the command exits non-zero; if it is alive but still not ready after 15s, a note is printed and the command exits 0 (the daemon keeps coming up).

### `bro stop`

```
bro stop [-c <path>] [--socket <path>]
```

### `bro restart`

```
bro restart [-c <path>] [--socket <path>] [--log <path>]
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

`bro status` reports the installed and running daemon versions when the daemon
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
it is idle; finish its work and run `bro restart` explicitly.
Foreground `bro serve` remains under its external supervisor; the CLI does not
take it over automatically.

Every `core` build holds daemon admission for its managed runtime's lifetime,
including core-only sessions executed by an external harness. Channel work,
pending checkpoint acknowledgements, native requests and queue runners do not
yet participate in an atomic handoff, so even an apparently idle core resident
defers automatic replacement. Gateway and harness-only builds retain tracked
idle handoff. Finish managed work and use the explicit `bro restart` when
changing binaries.
That exception applies to the same compiled distribution. A cross-profile
restart still requires atomic idle evidence and is refused by the managed
lifetime guard; plan an explicit stop after finishing work before starting the
other profile. No command silently removes that guard to change profiles.

`bro update` attempts the same safe handoff after a self-managed installation.
Package-manager installs still delegate the binary update; the next
daemon-dependent invocation checks compatibility. `bro update --check` and
`bro status` do not restart the daemon. `bro run AGENT PROMPT --no-start` will diagnose a
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

### `bro reload`

```
bro reload [-c <path>] [--socket <path>]
bro --context <name> reload
```

Hot-reloads supported configuration participants without dropping connections. Local IPC, `SIGHUP`, and remote reload use the same restart-required classification: changes to startup-owned settings (such as listen/control, plugins, or named routers) are rejected before any live participant changes. Restart the daemon to apply them.

Provider API keys in the current local environment are forwarded into reload preparation. `export OPENAI_API_KEY=…; bro reload` applies supported credential changes when the coordinated reload succeeds; startup-owned changes still require restart.

With `--context`, reload submits a guarded operation against the server's own
current configuration. It does not forward client environment variables. The
report includes a request id, server instance, generation, and per-subsystem
result. A running, failed, partially applied, or unknown operation exits
non-zero; use its operation lookup details and the target's state before retrying.

### `bro status`

```
bro status [-c <path>] [--socket <path>]
bro requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]
bro requests --human           # the same, as a table
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
| `named_policy` / `access_policies` | separate evidence for policy-lock and access-policy files; unchanged YAML alone does not establish synchronization |
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

`bro status --json` gained `spend` additively; every pre-existing key is unchanged.

`bro requests` reports what the router has actually done: newest-first
settled requests — time, model, the provider that **actually** served, tokens
in/out, cost, latency, status — plus daemon state and the window's spend and
trailing-minute rate. It reads the metering store directly, so it also works
with **no daemon running**. `--since` and `--until` are an RFC3339 pair with a
maximum seven-day interval; absent both, the server selects today from UTC
midnight. `--model` and `--provider` filter the same report before the limit.
`status --requests` remains a hidden compatibility spelling.

Like every other report it uses JSON by default and `--human` for the table. Repeat it with `watch -n1 bro requests --human` for a live view.

The spend rollup covers every caller of the daemon, not one session. Attributed ACP usage and retained run events provide the per-session figure.

**Spend is reported only where there is evidence.** An unpriced request shows `?`, not `$0.00`; the same honesty rule applies to `code <agent>` session cost.

Each row also carries `episode_id` — the trajectory episode to hand to `bro trajectory inspect`, or `null` when trajectory capture recorded nothing for it (capture is opt-in and off by default, so `null` is the common case). It is the thread from a settled request to its structural record, which is otherwise reachable only by an episode id nothing else hands out.

Portable — there is no terminal-only path left to gate.

> **`--requests` emits JSON by default as of 1.0.0-alpha.28.** It previously printed the table unconditionally, ignoring `--json` — the only `status` path that did. Scripts that parsed the table need `--human`; anything that wanted the data now gets one clean JSON object with a stable `rows[]`.
>
> **Replaces `--watch` (`-w`), removed in 1.0.0-alpha.28.** That flag opened a self-refreshing ratatui view with cursor keys plus `r` (reload) and `e` (`$EDITOR` on `bitrouter.yaml`). Both of those keys ran commands you can still run directly — `bro reload`, and your editor — and the piped form of `--watch` printed what `--requests --human` prints now.

---

## Config

### `bro config migrate-routers`

Preview conversion of block-form `presets:` into named `routers:` without
changing the source. The candidate preserves comments, request defaults,
provider preferences and policy bindings. Policy-lock contents, credentials,
`chat.model` and runtime mode stay unchanged.

```bash
bro config migrate-routers -c bitrouter.yaml --candidate routers.candidate.yaml
# Review the candidate diff, then use source_digest from the preview report:
bro config migrate-routers -c bitrouter.yaml --candidate routers.candidate.yaml \
  --apply --source-digest 'sha256:...'
```

Apply rejects a changed source or edited candidate, keeps a separate backup,
and replaces the source atomically. Restart the daemon to activate new routers.
Candidate and backup files can contain the same secrets as the original config;
they are local files, not included in report JSON. Unsupported YAML layouts,
invalid legacy names, or incompatible existing routers need an explicit manual
edit; migration never silently renames a caller-visible address.
Pass the actual file path for symlink-backed configs. CRLF is preserved; mixed
line endings must be normalized before migration.
Existing `@name[:variant]` calls continue to work during the compatibility
window. New callers should use `bitrouter/name`; canonical addresses do not
accept variants, except the existing `bitrouter/auto` compatibility entry.

Router authorization retains the existing requested-selector contract. An
access policy that grants `bitrouter/coding` delegates model choice to coding;
granting only a physical model does not grant the router alias. The current
pipeline does not recheck the model allowlist after router selection.

`bro config validate` reports named-router counts and legacy migration guidance.
The routing binding digest in diagnostics excludes prompt and parameter values;
it cannot identify changes confined to those values. It is distinct from the
policy artifact digest and from the exact-file checksum used by migration.

`bro models` adds a router inventory alongside the compatible model catalog.
`bro status` distinguishes `saved_routers`, `running_routers`, and the optional
`router_restart_required` result; unavailable runtime evidence stays unknown.
`bro route bitrouter/coding` reports `router`, `router_source`, `bound_policy`,
and routable `candidate_models`. A dynamic policy preview reports
`policy_decision_executed: false`, labels `effective_model` as the base input,
and leaves `provider_chain` empty: it has not executed the policy or selected
an upstream. Settled request history records optional `router_id`,
`binding_digest`, and `original_selector` beside the actual provider/model.
Old rows keep null identities; these fields are not exported as new trace
attributes in this batch.

### `bro init` (onboarding wizard)

```bash
bro
bro init --yes --harness codex --after exit
bro init --harness claude --model anthropic/claude-sonnet-4-6
```

Bare `bro` ensures the selected local daemon is running in the background, waits
for its control-socket readiness response, emits one JSON report (`--human` is
also available), and returns to the shell. Repeated or concurrent invocations
reuse that daemon. A source-verified running target also remains reusable when the client no longer has its startup credentials or YAML. A valid Router configuration or detected zero-config credentials
is sufficient; `chat.agent` is optional. The bare result has `action: ensure_start`,
`status: ready`, `pid`, `listen`, `models`, `log`, and optional `config_state`.
Detection does not verify provider login
or connectivity. Explicit remote contexts require an administration subcommand.

Interactive setup lists every active, public provider from the registry in one
alphabetical list. BitRouter Cloud (`bitrouter`) is an ordinary provider row.
Startup uses that source's valid cache or the source-bound packaged default as
whole alternatives, so a fresh installation has a default catalog offline.
Custom sources never borrow the packaged default. Background refresh starts
after readiness and updates subsequent routes; disable it with
`registry.auto_refresh: false`. Configured credentials are marked.

When setup is missing, stdin and stderr must both be terminals. First-run setup
continues to background startup after saving. Without those terminals, missing
setup produces an actionable error and a nonzero exit; it never waits for input.
Redirecting stdout is supported. Existing configuration resolves from
`./bitrouter.yaml`, `$BITROUTER_HOME/bitrouter.yaml`, then
`~/.bitrouter/bitrouter.yaml`; onboarding creates a missing file in the selected
BitRouter home. `init -c PATH` selects an explicit destination.

The three steps are Providers, optional Agents/resources, and Finish. Registry
providers and login methods remain selectable; retry a failed login or add more
providers before choosing **Continue to agent setup**. A custom or disabled
registry remains authoritative. Enter a number or exact displayed name to select,
or submit a keyword to filter; `/` restores all choices. Numbers remain stable
while filtering; an empty line accepts the explicitly shown default. Reset
confirmations use `[Y/n]` or `[y/N]`. The terminal owns normal line editing;
there is no alternate screen, raw-key menu or redraw framework. Secrets and pasted
OAuth redirects have echo disabled. EOF and Ctrl-C cancel and restore input state.
Router progress is saved after provider/agent selection, before optional resources;
cancelling a resource step preserves that saved setup and does not run Finish.
Prompts and diagnostics go to stderr.

The optional resources menu offers **Add MCP server** and **Continue**. It uses
the same source-owned MCP transaction as management commands. Saved progress
survives failures; use paired `--mcp-name NAME --mcp-from FILE|-` headlessly.
The wizard owns stdin, so use an entry file there. Failure emits one partial
report and suppresses Finish; saving MCP does not connect or grant skill consent.
Install skills with `npx skills` and inspect/write their existing directories
with `bro skills`. Onboarding has no skill installation or Git-ref step.

`init --yes` is an explicit scaffold/save operation and exits by default. No agent
is chosen automatically. `--harness claude|codex` selects an optional ACP default;
the first repeat is saved as `chat.agent`. `--model ID` preserves an optional
`chat.model`. Skipping the agent step preserves existing settings. Selection does
not install, initialize or connect an agent. `--after serve` waits for a ready daemon;
`--after launch` explicitly launches the selected **native** Claude/Codex tool with
inherited terminal I/O (the tool then owns stdout). Headless setup refuses a missing native executable instead of asking to install it; a failed optional launch keeps the saved Router setup. Existing YAML values are
preserved; `--force` explicitly resets them to the starter configuration. Writes
are atomic. Starter defaults remain `127.0.0.1:4356`, `skip_auth: true`.

For bare `bro` and `init --after serve`, a timeout is an error rather than a readiness claim; the pending process may
continue starting. Cancellation after spawn has the same boundary. Check `bro status`
and `bitrouter.log`; another bare call waits on the pending process rather than
spawning a replacement. An early child exit is reported with this run's log tail.
An existing daemon is never implicitly reloaded or restarted: its reported
`config_state` preserves saved/running differences, including socket changes.
A closed stdout pipe ends reporting with a nonzero exit and does not stop a daemon.

| Flag | Description |
| --- | --- |
| `-c`, `--config PATH` | Configuration destination; otherwise use the resolution chain above. |
| `--yes`, `-y` | Process flags without interactive setup prompts. |
| `--force` | Reset existing configuration to starter defaults before saving. |
| `--reset` | Clear BitRouter credentials before setup; does not remove vendor CLI credentials. |
| `--cloud-login` | Cloud device login; reported-and-skipped headlessly. |
| `--api-key KEY` | Seed a BitRouter Cloud API key. |
| `--provider ID` | Log in to a provider (repeatable); interactive OAuth is skipped headlessly. |
| `--provider-api-key KEY` | Key paired with the provider at the same position. |
| `--use-detected` | Accept detected credentials. |
| `--harness claude\|codex` | Optional ACP adapter default; no automatic installation or connection. |
| `--mcp-name NAME`, `--mcp-from FILE|-` | Paired optional MCP entry; same validation, CAS/conflict and publication contract as `mcp add --from`. |
| `--after launch\|serve\|exit` | Explicit native launch, ensure a ready daemon, or save and exit. |
| `--model ID` | Persist the optional default model, also used by native launch. |

The JSON envelope includes `action`, `providers_configured`, `providers_skipped_interactive`, `agents_selected`, `resources`, `after`, `snippet`, and `config_activation: saved_only`. The deprecated `harnesses_installed` is always empty. `--after serve` also includes a `daemon` readiness report with its saved/running `config_state`; saving alone never proves activation. Connection snippets reference shell variables rather than printing exported secrets.

Retryable route candidates advance immediately unless an operator opts into an
upstream fallback delay schedule:

```yaml
upstream:
  fallback_backoff_ms: [1000, 2000, 4000, 8000, 16000, 30000]
```

The first value applies before the second candidate, the second before the
third, and the final value repeats if the configured chain is longer. The
schedule applies only after an error the fallback policy classifies as
retryable; it does not retry non-retryable 4xx responses and does not change
route selection. An empty or omitted schedule preserves the existing behavior.

---

## Routing / introspection

### `bro route <model>`

```
bro route gpt-4o [--prompt <text>] [-c <path>] [--socket <path>]
```

Resolves a model or router selector using the running daemon when reachable, otherwise the local configuration. Fixed routes include the provider fallback chain. A policy-bound router reports its binding and routable candidates with `policy_decision_executed: false`: the preview does not execute its dynamic policy or predict the selected model.

The config fallback probes `auto_discover: true` providers with no declared
models, using the same bounded discovery as `bro models`. A model shown by the
config listing can therefore be previewed by the config route check.

`--prompt` supplies request text for the existing static policy-table preview on the local config path. It does not execute a router's dynamic policy; live previews do not use it.

`bro route --json` returns the shared route action report:

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

### `bro models`

```
bro models [-c <path>] [-p <provider-id>]
```

Lists all routable models, each with **every** provider that can serve it — the
fallback chain, in order. Filter to one provider with `--provider`.
Subscription providers are explicit-route-only, so their rows use a pinned
`provider:canonical-model` selector (for example,
`openai-codex:openai/gpt-5.6-sol`). Copying any displayed selector into
`bro route` therefore previews the route without implicitly opting a bare
canonical request into a personal subscription.

Queries the running daemon if reachable and falls back to a local config parse,
the same order `bro route` uses: the live routing table reflects `reload`s
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

### `bro providers list`

```
bro providers list [-c <path>] [--socket <path>]
```

Local compatibility output prints each provider's id, model count,
routing-active state, and `api_base`. Named remote contexts and the dashboard
use the redacted accepted catalog, which omits API bases and credentials.
`active` means the accepted routing configuration includes the provider, not
that a connectivity probe succeeded.

---

## MCP upstream diagnostics

### `bro mcp check [server]`

```bash
bro mcp check                 # every configured upstream
bro mcp check my-server       # one configured upstream
```

Performs one `tools/list` round trip per selected server and reports transport,
reachability, latency, negotiated tools capability, and advertised tool names.
This is the canonical diagnostic. The `tools` commands below are hidden
compatibility spellings.

### Hidden compatibility: `bro tools`

### `bro tools list`

```
bro tools list [-c <path>]
```

Connects to every `mcp_servers` entry in the config and lists advertised tools with descriptions.

### `bro tools status`

```
bro tools status [-c <path>]
```

Health-checks each configured MCP server with a `tools/list` round-trip. Prints status, latency, and transport.

### `bro tools discover <server>`

```
bro tools discover my-server [-c <path>]
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
`bro status` reports these saved changes under the fixed `skills` restart field;
routing reload rejects additions and revocations without replacing the live policy.

`bro mcp list` lists **configured upstreams**; `show NAME` shows one. Both
read the source without expanding environment references, connecting to MCP,
or starting a daemon. JSON reports include the absolute `source` path and
redacted `servers` with each source map key as `id` and its declared `name`.
Old aliases remain readable; edits require matching key/name and never rename
another entry implicitly. Human output uses `--human`. Header/env values and stdio
argument values are hidden; HTTP userinfo, query and fragment are omitted.

```sh
bro mcp list [--config PATH]
bro mcp show NAME [--config PATH]
bro mcp add NAME --url https://example.com/mcp [--config PATH]
bro mcp add NAME --config PATH --command program -- arg1 'two words'
bro mcp add NAME --from entry.yaml [--config PATH]
bro mcp edit NAME --from entry.yaml [--config PATH]
bro mcp remove NAME [--config PATH]
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
running bindings. Use `bro status` for the selected daemon's own config-state
evidence. Native MCP transport/credential changes require an explicit restart
after active work finishes; ordinary routing reload cannot apply them.

`bro mcp check [server] [--config PATH]` explicitly connects to one configured
upstream, or all, and reports transport, latency, capabilities and tool names.
`bro tools list/status/discover`, aggregation, Skills-over-MCP relay and native
launcher injection retain their existing behavior. OSS does not expose a
first-party origin MCP server.

The hidden legacy `bro mcp search QUERY [--limit N]` still browses the official
registry. Search is separate from configured-server management. Former hidden
registry `list`/stub-only `add` semantics are replaced by the commands above;
search results do not save or install an entry.

---

## ACP agent management

Bare `bro agents` returns a read-only snapshot from the owner-local daemon;
it works in a pipe and never starts a daemon, agent, or MCP server. Use
`--config PATH` or `--socket PATH` to select the target, `--limit N` (1–16) to bound
the page, and `--after NEXT_CURSOR` to continue within the same daemon boot.
Options for catalog/session subcommands follow that subcommand. Named remote
contexts are rejected; the command never falls back to local execution.
Before onboarding, an absent YAML uses the conventional endpoint without
creating files. Existing invalid configuration errors unless a verified
runtime locator already identifies the running source.

`daemon_state: running` with `instances: []` means zero in the observed scope.
`daemon_state: stopped` with `instances: null` means the endpoint is absent or
refused the connection. Timeout, unsupported protocol, permission, and decode
failures return errors, never an empty list. A minimal embedding with no
inspection owners returns `instances: null` and unavailable coverage.

Each instance has a `kind`: `native_worker`, `supervised_acp`,
`native_acp_attachment`, `managed_core_channel`, or `managed_native_session`. IDs retain Thread/Turn,
run/native-session, connection, parent-run, or harness/epoch bindings where
known. Idle ACP sessions remain connected; stopped/failed history, cold
Threads and old leases do not. `model` uses confirmed route evidence where
available; a native Turn's requested selector is separately `model_selector`.
Unknown model/resource fields are null (human: unknown). The snapshot exposes
no transcript, permission input, credentials, or skill body.

`coverage` states which owners were observed. Independent ACP controllers,
independent NativeSession library embeddings, external native launchers and
inference-only traffic have no daemon liveness proof. Product `task managed`
drivers appear as `managed_native_session` until cleanup/release; retained
journals alone do not prove liveness. Native ACP coverage is attached sessions,
excluding initialized connections without a session. Worker and attachment rows
may bind the same Thread and describe different facts. Sampling across owners
is not atomic; pages use kind/instance ordering and fresh membership, not a
frozen historical result. A cursor from another daemon boot is rejected.
`agents list` retains catalog/config semantics; `agents sessions` retains runs.

### Supervised sessions

```console
bro agents                         # connected instances (JSON; --human available)
bro agents sessions [--json]       # scriptable run inventory
bro agents show <agent-run-id>     # inspect retained state
bro agents events <agent-run-id>   # append retained/live events
bro agents stop <agent-run-id>     # stop controller/child; keep native session
bro agents remove <agent-run-id>   # remove settled BitRouter metadata/events
```

`attach` acquires the run's generation-fenced interactive lease. Detaching
keeps the controller, native session, active turn, route lease, and metering
alive. `stop` is a separate mutation and does not delete harness-native saved
state. `remove` is accepted only after a stopped, failed, or interrupted child
has been reaped; it deletes the minimal BitRouter ledger row and ephemeral
retained events, not native harness history.

The session list derives **Needs input**, **Ready for review**, **Working**,
**Idle**, and **Stopped** from typed supervisor state. A settled ACP turn is
Ready for review, not proof that the task is complete. A daemon restart marks
formerly live ledger rows Interrupted and never silently relaunches them.

The owner-only local session socket issues exact, command-scoped grants.
`sessions` receives metadata-only list authority: it cannot read transcripts,
raw tool or permission context, prompt/result content, exact failure text, or
mutate a run. Peek, retained transcript, attach, respond, stop, remove, and
start are distinct scopes; interactive clients request only the set their
surface can actually exercise.

### `bro agents list`

```
bro agents list [-c <path>]
```

Shows the built-in agent catalog alongside which agents are configured in the loaded config.

### `bro agents inspect`

```bash
bro agents inspect claude
```

Opens a fresh harness-native session, waits briefly for its advertised slash
commands, and reports which source answers each command.

### `bro agents check`

```
bro agents check [agent] [-c <path>]
```

With an agent or friendly alias, preflights that adapter and routing target.
With no agent, spawns each configured adapter and verifies `initialize`.

### `bro agents scaffold <id>`

```
bro agents scaffold claude-code
```

Prints a YAML stub for the named catalog or registry agent. Paste the output
under `agents:` in `bitrouter.yaml`. `agents install` remains a hidden alias.

### `bro agents conformance <id>`

```
bro agents conformance local/claude-acp
```

Runs the `acp_compat_1` ACP-compatibility suite and prints the `conformance:`
block to record under the agent's entry in `registry/runtimes/<runtime>.yaml`.
`<id>` is `<runtime>/<harness>`; `local/` is the default runtime and may be
elided.

Two tiers. **handshake** — the agent answers `initialize` and settles on the
ACP version its registry entry declares. **routability** — the agent's LLM
traffic reaches BitRouter when its routing block is applied, carrying the
gateway credential and the pinned model.

No provider credentials are needed: the agent is launched with its own routing
pointed at an ephemeral loopback gateway that records what arrived. It does
spawn the agent, so the package or binary must be installed. A harness routed
only on the interactive launch path (opencode, pi, hermes, openclaw) reports
routability as `skipped` — its ACP facet launches direct, so there is no routed
ACP traffic to observe. Exits non-zero when a tier fails or when nothing was
verified, and prints no record in either case.

### `bro run` — headless agent

```
bro run <agent> [prompt|-] [--prompt-file PATH] [--load ID|--resume ID]
              [--cwd PATH] [--format ndjson|text|quiet]
              [--approve-all|--approve-reads|--deny-all]
              [--permission-policy JSON|@PATH] [--result-schema JSON|@PATH]
              [--background [--allow-shared-directory]]
              [--turn-timeout <secs>] [routing flags] [-c <path>]
```

The canonical always-headless agent surface. It opens one ACP session, sends
one prompt from the positional argument, a file, or stdin, and streams NDJSON
by default; text and quiet formats are explicit. `--load` replays a native
session's history and `--resume` continues without replay when advertised.
Every NDJSON event carries `version: 1` and a monotonic `seq`; `json` remains a
temporary format-value alias for `ndjson`.
Permissions, result validation, routing, timeouts, session identity, and exit
codes are the same implementation used by the compatibility `acp prompt` and
`spawn <agent> -p` forms.

`--background` is a different lifecycle, not an alias for hidden `--no-wait`.
It ensures the local daemon is available, submits the run to its resident
supervisor, and returns only after the supervisor owns an attachable ACP
session and can return an agent run ID. The turn continues when this CLI exits.
`--no-wait` conflicts with `--background`.

Foreground `run` remains deny-all when no permission option answers an
unmatched request. A background run has an interactive broker, so its unmatched
default is **Ask**: the run enters Needs input until an authorized client
responds. `--approve-all`, `--approve-reads`, and `--deny-all` keep their exact
existing meanings. A permission policy's explicit `defaultAction` wins; a
policy without `defaultAction` applies its exact matches and otherwise uses an
explicit mode or background Ask.

`--turn-timeout` includes time waiting for a background permission and records
a timeout failure after cancellation. `--result-schema` is validated before
submission and the settled reply must satisfy it before the row becomes Ready.
`--load` and `--resume` remain capability-gated explicit native-session
selections; daemon restart never auto-resumes an interrupted row.

Every supervised run claims the canonical Git worktree root, or the canonical
requested directory outside Git. A second potentially writable run with the
same claim is rejected even when it requested another subdirectory. Prefer a
separate worktree. `--allow-shared-directory` is a background-only, explicitly
warned override; it disables this single-writer protection but does not broaden
agent permissions.

### `bro acp`

```bash
bro acp serve [--model MODEL] [--read-only] [--turn-timeout SECS] [--no-start] [-c PATH]
bro acp serve <agent> [routing flags] [--turn-timeout SECS] [-c PATH]
```

Bare `acp serve` exposes native BRO ACP v1 or draft v2 over protocol-pure stdio.
It selects `--model` or `chat.model` and joins the local daemon's existing
ThreadService; EOF detaches without cancelling accepted work. `--read-only`
uses native read tools only. `--turn-timeout` sets the native execution deadline
(1–86400 seconds). `--no-start` requires an existing daemon. `--direct` and
`--base-url` require an explicit external agent.

Native `sessionId` is the durable Thread ID. v1 prompt returns at settled
completion; v2 returns its user Item `messageId` at acceptance and reports state
later. Same-daemon reconnect redelivers the same native approval request; native
restart recovery cannot reuse the old approval sender. `session/cancel` cancels
active work while preserving a paused accepted queue. Load/resume reattaches;
`_bitrouter/session/resume_queue` explicitly resumes retained inputs. A new
foreground prompt can clear a safely settled empty pause. `session/close` cancels
all accepted inputs, waits for cleanup and retains history, then requires reopen.
Use `_meta.bitrouter.idempotencyKey` for retryable durable operations. See
[the native ACP contract](BRO_NATIVE_ACP_SERVER_SPEC.md) and its
[acceptance record](BRO_NATIVE_ACP_SERVER_ACCEPTANCE.md).

MCP requests must exactly match host-configured bindings; native resources,
skills and AGENTS.md use the existing Harness. Client file/terminal callbacks,
arbitrary MCP programs, images/audio, remote ACP and a mutable model picker are
not supported in this release.

Explicit `<agent>` retains the external controller path: one connection can
carry multiple harness-native sessions, with IDs/history owned by the harness.
Hidden `acp prompt` and `spawn` spellings remain for migration.

### Removed first-party UI

`code`, `tui`, `chat` and `agents attach` only return a migration error, before
configuration, context lookup or daemon I/O. They are absent from normal help.
Use `threads` for native controls; `run` for a single ACP prompt;
`agents show/events/pending/answer` for supervised state and approvals;
`launch <agent>` for a harness's native interface.

Existing `chat.model` and `chat.commands` remain headless defaults. `chat.agent`
only selects a harness for explicit onboarding `--after launch`; it does not
change bare `bro` into a conversation. `config validate` explains this legacy
namespace when a saved agent exists. The old `code-hotkeys.json` file and UI
bindings are no longer read. Configuration, native history, metering and saved
ACP sessions are retained; migration never rewrites or deletes them.

### ACP workers, local CLI discovery, and native launch

Daemon-supervised Codex and Claude accept repeatable `--skill project:ID` or
`--skill global:ID`, and `--exclude-skill SELECTOR`, on `run --background`.
For example, after preparing a directory, obtain its identity with
`bro skills list` and use
`bro run codex "review changes" --background --skill project:DIRECTORY_ID`.
These constraints remain on that run; each subsequent `agents prompt` performs
fresh discovery and explicitly binds those directories. With no includes, no
skill is selected; the configured decision backend is never called for this.

Codex receives verified `SKILL.md` as ACP text blocks. Claude receives embedded
text resources only when that connection advertises `embeddedContext`. Both
carry the directory path, frozen file manifest/content fingerprint and exact
ACP session/input identity. Changed bytes reject an old binding. The request is journaled before
dispatch; at most four selected frames / 64 KiB of instruction text and 2 MiB of
serialized ACP request are admitted, without truncation. The original supervisor
ledger retains at most 64 reports / 4 MiB per run and fails when capacity is
exceeded, rather than discarding earlier evidence.

`agents list` reports path contracts in `skill_delivery`; it does not probe live
readiness. `agents show RUN` reports negotiated `capabilities.skill_delivery` and
per-input `skill_deliveries`, including explicit constraints, candidate reasons,
prepared receipt and delivered/cancelled/uncertain status. Unknown results are
not replayed; cold queries restore facts, not an executable agent owner.

These carriers prove local prompt-instruction delivery. They do not promise
native skill activation, protected bundle/support-file access or vendor model
context admission (`model_context_budget_verified: false`). Model token limits,
history retention and compaction remain external-agent responsibilities. A later
request omitting a skill does not prove its removal from earlier vendor history.
`allowed-tools` grants no permission; ACP approval remains independent.

Required MCP-origin selectors are `unsupported` before dispatch, including with
`--approve-all`: these carriers cannot preserve MCP host manifest, consent,
cache and execution responsibility. Foreground/standalone/transparent ACP,
unknown adapters, native launch and inference-only routing retain their existing
operations but do not support this daemon skill allocation. A native slash
command retains its original single-text shape and skips skill delivery;
combining it with a required skill is explicitly unsupported. No global harness
configuration or vendor install is part of allocation. Local independent Rust
fixtures cover both carriers; real vendor sessions remain unverified.

The maintained adapters are pinned to `@agentclientprotocol/codex-acp@1.10.0`
and `@agentclientprotocol/claude-agent-acp@0.75.1`. Node.js 22+ and `npx` are
required; npm obtains the pinned adapter on first use. No `agents:` entry is
needed for either built-in id.

Codex and Claude subscription login metadata and model defaults also ship in
the binary. With the default public registry, the packaged document is used
only when no usable source-bound cache exists; remote and packaged inventories
are never merged. A custom registry URL or `registry.enabled: false` opts out.
A stored subscription login
auto-enables its provider, so no manual `providers:` entry is required.

When `openai-codex` is active, Codex ACP can use its native default model
without `chat.model` or `--model`. Requests marked by the maintained adapter
map declared native model names to the Codex subscription at gateway ingress;
the CLI retains its native model metadata and picker. Generic API requests do
not opt into subscriptions this way. Explicit provider/canonical model names,
presets and user-defined virtual models are preserved. Reloading the daemon
updates the native-model mapping along with the active provider catalog.

At ACP session startup, BitRouter probes local CLIs with `--version` (two-second
limit). Codex >=0.153.3 is passed to its adapter via `CODEX_PATH`; Claude Code
>=2.1.257 is passed via `CLAUDE_CODE_EXECUTABLE`. Missing, old, failing, or
unresponsive CLIs leave the adapter's bundled runtime in use. Explicit env and
agent-transport overrides win. Custom adapter versions are not modified. The
ACP adapter remains the protocol peer; the native launch commands below are a
separate compatibility facet.

```
bro launch <agent> [--model <id>] [-c <path>] [--base-url <url>] [--no-install] [--no-start] [--check] -- <agent args…>
bro claude [options] -- <claude args…>
bro codex [options] -- <codex args…>
```

Launches a coding-agent harness as an **interactive native-TUI** child process with its gateway base URL pointed at BitRouter, so the agent's traffic routes through the router **without touching the agent's own config files**. This is the native-harness compatibility surface — the human drives the harness's own TUI; for a headless prompt use `bro run`.

Before handing over, `launch` prints one line stating what the harness actually got — whether it is routed, and whether the tools/skills gateways reached it. That ceiling is the harness's, not BitRouter's: `pi` exposes no MCP mechanism to inject into.

```
launch: claude · routed via bitrouter (http://127.0.0.1:4356) · tools ✓ skills ✓
launch: pi · routed via bitrouter (…) · tools ✗ skills ✗ (pi has no MCP mechanism)
```

The positional agent takes any catalog harness with an interactive binary. `claude` and `codex` also have top-level shortcuts; `claude-code` aliases `claude`. The old `-a/--agent` spelling remains hidden during migration. Each harness is routed by its catalog mechanism:

| Harness | How it reaches BitRouter |
| --- | --- |
| `claude` | child env (`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL` for `--model`) |
| `codex` | one-shot `-c` overrides for a `bitrouter` provider (`base_url = <target>/v1`, `wire_api = "responses"`) |
| `opencode` | synthesized `OPENCODE_CONFIG` JSON declaring a `bitrouter` openai-compatible provider |
| `pi` | synthesized `PI_CODING_AGENT_DIR` with a `models.json`, selected by `--provider bro --model …` |
| `hermes` | synthesized `HERMES_HOME` with a `config.yaml` (loopback `custom` provider + `CUSTOM_API_KEY`) |
| `openclaw` | synthesized `OPENCLAW_STATE_DIR` + `OPENCLAW_CONFIG_PATH` profile (run as `tui --local`) |
| `grok`, `agy` | **not routed** — own-auth subscription clients (see below) |


The synthesized files are throwaway, written under the working tree's self-ignoring `.bitrouter/launch/`; the user's own `~/.config` is never touched. Their model lists come from the daemon's `/v1/models` (best-effort — an unreachable daemon just yields an empty list and the harness keeps its own defaults).

**Gateway MCP server.** `launch` injects `bitrouter_tools`, the daemon's aggregate endpoint at `mcp.aggregate.route`, so configured upstream MCP servers reach the harness; it is omitted when `mcp.aggregate.enabled: false`. Injection reaches harnesses with an MCP mechanism — `claude` (`--mcp-config`), `codex` (`-c mcp_servers.*`), and `opencode` and `hermes` (their synthesized config files). `pi`, `openclaw`, `grok`, and `agy` expose no injectable MCP surface. Installed local skills are supplied by the agent host's normal skill/plugin rails, not by a BitRouter origin server.

`--model <id>` pins the harness's model through whatever mechanism it has. Following `cargo run`'s convention, everything after `--` is forwarded verbatim, e.g. `bro launch claude -- -p "summarize" --dangerously-skip-permissions`.

**`grok` and `agy` are own-auth harnesses.** They launch with their own subscription auth and are **never redirected** — the startup line says `own-auth · not routed · not metered`, and `--check` reports it as a `routing` warning. The Grok subscription can still serve routed requests as `supergrok`. The private `google-ai` provider is retired; launching `agy` with its own authentication remains independent of the router.

The agent authenticates to BitRouter with `BITROUTER_API_KEY` when set; otherwise a local placeholder is used (fine under the `skip_auth` default written by `bro init`). A missing `claude` / `codex` binary is offered for install via its official native installer (`--no-install`, or a non-TTY stdin, declines); the other harnesses have no bundled installer and error with a pointer to their upstream project.

When the target is the local daemon (a derived base URL on a loopback/wildcard bind) and none is running, `launch` **auto-starts it** — printing a hint, launching a detached `serve`, and waiting for readiness before handing off to the agent. Pass `--no-start` to skip this (a reachability warning is printed instead). An explicit `--base-url` or a non-local bind is never auto-started — BitRouter can't start someone else's daemon — and only gets a warning if it looks unreachable.

After the wrapped agent exits, `launch` prints a one-line session spend summary to stderr (spend during the run + today's total, from the local metering database). Silent when nothing was recorded in the window — e.g. when the run targeted Cloud.

`bro spawn --agent <claude|codex>` is a **deprecated alias** for `launch` (prints a migration note); it will be removed after one or two alpha releases.

### Hidden `spawn` compatibility

The old `spawn` umbrella remains parseable for one migration window but is
absent from normal help. Its modes call the canonical implementations:
`spawn <agent> -p TEXT` maps to `run`, `spawn <agent> --serve` maps to
`acp serve`, and `spawn <agent> --check` maps to `agents check`. New scripts
and integrations must use the canonical commands above; protocol-serving
compatibility forms keep stdout reserved for ACP frames.

### `bro policy`

```text
bro policy init NAME [--router ROUTER] --economy MODEL \
  [--economy-effort LEVEL] [--strong MODEL] [--strong-effort LEVEL]
bro policy init NAME --preset PRESET --economy MODEL [--economy-effort LEVEL] \
  [--strong MODEL] [--strong-effort LEVEL] # legacy compatibility
bro policy check|status|show [--config PATH]
bro policy compile --output FILE [--eval-snapshot SHA256] [--snapshot-time UNIX_MS]
bro policy diff ACTIVE CANDIDATE
bro policy publish CANDIDATE [--config PATH] [--socket PATH]
bro policy verify --evidence [--config PATH]
bro policy evolve [--config PATH] [--apply | --output FILE]
bro policy reload [--config PATH] [--socket PATH]
bro policy rollback DIGEST [--config PATH] [--socket PATH]
```

The BitRouter process, not the policy lock, owns adaptive behavior:

```yaml
policy:
  path: ./policy-lock.yaml
  mode: frozen # or adaptive
```

```bash
bro policy init coding \
  --strong provider:strong-model \
  --economy provider:economy-model
```

This creates the `coding` policy and router, addressed as `bitrouter/coding`.
Omitting both binding flags defaults `--router` to `coding`; another router
requires `--router ID`. The command never chooses either model or provider. A
new router requires both model arguments, while a compatible existing policy
router supplies its recorded strong base model when `--strong` is omitted.

Router initialization preserves the existing `policy.mode` and all chat or
harness settings. With no explicit mode, the configuration remains `frozen`.
Live routes use only the signed lock; Eval rows never change request routing on
their own. Operators can set `mode: adaptive` when explicit publication is
allowed. Invoking `optimize run` is explicit authorization to activate adaptive
mode and autonomously publish its successor when the controller decides to do
so. Dry-run compilation and candidate export remain available. The mode
controls write authority, not request-time learning.

`--preset PRESET` is the compatibility initializer for legacy `@preset`
configuration. It remains mutually exclusive with `--router`, can infer the
strong model from an existing preset, and retains its historical behavior of
setting `policy.mode: adaptive`.

`policy publish` promotes the exact compiled v3 candidate after validating its
parent digest, certificates, and current config. A stale candidate or frozen
process leaves the active bytes unchanged. `policy evolve --apply` remains the
legacy migration shortcut; use `compile` + `publish` whenever an eval snapshot
is part of the candidate lineage.

The lock contains deterministic routes, tiers, and learning thresholds, but no activation or freeze switch. Older `policy.writeback: locked|evolve` input remains readable as `frozen|adaptive`; newly written configuration uses `policy.mode`. The old `policy lock`, `policy unlock`, and `policy evolve --freeze` surfaces have been removed.

### `bro optimize`

```text
bro optimize run [--policy auto] [--candidate-tier TIER] \
  [--objective cost|latency] \
  [--exploration-ppm 100000] [--minimum-tasks 3] [--maximum-tasks 20] \
  [--minimum-pass-rate-ppm 900000] \
  [--evaluator-config-digest sha256:...] \
  [--config bitrouter.yaml] [--socket PATH]
bro optimize status [--policy auto] [--config bitrouter.yaml]
```

Optimization is driven by history from normal use, not by a bundled workflow
runner. Initialize a policy, run a coding agent or Terminal Bench normally,
submit externally evaluated results, and advance the controller one step:

```bash
bro policy init auto --router auto \
  --strong provider:strong-model --economy provider:economy-model
# run the coding agent or Terminal Bench normally through bitrouter/auto
bro eval result submit result.json --config bitrouter.yaml
bro optimize run --policy auto --config bitrouter.yaml
bro optimize status --policy auto --config bitrouter.yaml
```

Repeat normal traced work, external Eval submission, and `optimize run` until
that command reports the controller decision `converged`. Use `optimize status`
to observe the signed policy state without changing files or the database: it
reports `exploring` while an experiment is active and `idle` otherwise, but
does not infer convergence from Eval history. Calling `optimize run` grants
autonomous authority for exactly one deterministic controller step: it may
publish `explore`, `promote`, or `retreat`, or leave the lock unchanged for
`hold` or `converged`. There is no separate review or publish approval.
Publication uses the current policy digest as a compare-and-swap parent and
reloads a reachable daemon; a stale parent or failed reload leaves or restores
the prior active state. When `--candidate-tier` is omitted, the controller uses
the signed policy's `adequacy.explore_tier`; pass `--candidate-tier TIER` only
to override it for that step.

The first run can cold-start signed exploration from champion-only history.
That history ranks opportunities by request frequency and cost contribution,
but cannot prove an unexecuted challenger is better and therefore cannot
promote one directly. Later runs use complete `task` or `episode` cohorts for
quality and cost. Request subjects help rank the next opportunity but never
enter the gate. Promotion requires the configured quality/pass gate, no hard
violation, and a lower mean complete-unit cost for the challenger. Complete
cost prefers `trajectory.cost.usd_micros` and accepts evaluator-authored
`cost.usd_micros` in micro-USD; per-request price does not gate promotion.

For unpriced subscriptions, `--objective latency` instead minimizes complete
task wall time with the same quality and hard-violation gates. The conclusive
task evaluator must report `latency.wall_ms` in `milliseconds` for every eligible
unit in both arms, including failures, measured from admission through
verification under comparable conditions. Missing or invalid measurements and
per-request `latency.ms` do not authorize promotion. The objective is signed
into exploration; later commands cannot change an active experiment's objective.
Omitting the flag retains the existing `cost` objective.

During exploration, router-authored decision evidence contains an optional
signed `experiment` reference with the experiment id, `control` or
`challenger` arm, `task` or `episode` assignment unit, assignment-id digest,
and challenger propensity. Evaluators must copy that object verbatim and must
never invent or edit it. Optimizer cohort membership comes from this router
evidence, not the evaluator-owned `cohort` string.

Router-authored decisions may also contain `route_measurement`. This versioned
object records every tier/model/effort target declared by the same immutable
policy snapshot, the semantic action chosen before tool, progress, or
continuation guards, and its logging probability in integer ppm. The ordinary
`selected_*` fields remain the effective post-guard route. Deterministic routes
use one million ppm; assigned experiments use the signed arm probabilities. If
an experiment lacks a stable task or episode identity, the router records a
deterministic champion action instead of inventing randomized evidence.

The generic Eval Exchange and low-level `policy compile`, `policy diff`,
`policy publish`, `policy rollback`, and `policy verify` commands remain
available for independent evaluation, migration, audit, and operator-managed
policy workflows. They are not extra approval stages for `optimize run`.

### `bro trajectory`

Durable trajectory progress control is an explicit local opt-in:

```yaml
trajectory:
  enabled: true
  retention_days: 30
  outbox_batch_size: 100
```

`enabled` defaults to `false`; `retention_days` defaults to `30` and must be
positive; `outbox_batch_size` defaults to `100` and must be between 1 and 1000.
A signed policy lock containing any `progress_guard` is rejected unless
trajectory capture is enabled. Every trajectory setting is restart-only:
changing `enabled`, `retention_days`, or `outbox_batch_size` during reload is
rejected while the last-known-good runtime remains active. Restart the daemon
to apply any trajectory setting change.

Progress-guard clauses have two timing models. `max_recovery_count` is an edge
trigger: it compares the prospective cumulative recovery count only when the
current request enters `recovery` from another projection. Consecutive
`recovery` projections remain protected but do not count or activate again; the
configured hold, not the cumulative counter by itself, determines how long the
escalation persists. The episode request, elapsed-time, and known-cost
thresholds are monotonic once reached.
Every genuine trigger activates hold. If the current candidate is any declared
protected tier, it is preserved exactly; otherwise the escalation tier is
selected. An active hold uses the same selection rule without resetting its
duration. Unknown cost remains unknown and cannot satisfy a cost threshold.

The feature is source- and task-neutral. It stores event structure, bounded
categorical routing facts, exact counters, and keyed/content digests. It does
not store API keys, bearer credentials, prompts, system instructions, tool
arguments, file bodies, or provider-private metadata. Operational Eval records
are redacted digest/count evidence and an `inconclusive` verdict; they are not a
quality score and do not infer task identity or capability from private data.

```text
bro trajectory inspect EPISODE_ID
bro trajectory replay EPISODE_ID
bro trajectory prune --before RFC3339 [--dry-run]

bro trajectory --config PATH inspect EPISODE_ID
bro trajectory --config PATH replay EPISODE_ID
bro trajectory --config PATH prune --before RFC3339 [--dry-run]
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
event id/sequence and a stable reason code; a pending route awaiting its guard
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

Progress metrics are descriptive: request/settlement counts, elapsed time,
projection/tier/unprotected streaks, recovery/hold counters, and optional
authoritative token/cost totals. Missing metering stays absent rather than
becoming zero. `history_complete=false` means the visible prefix is not proven
complete and guards follow their configured incomplete-history behavior.

### `bro eval`

```text
bro eval subject put FILE [--config PATH]
bro eval subject get EVAL_ID [--config PATH]
bro eval subject list [--config PATH]
bro eval result submit FILE [--config PATH]
bro eval snapshot freeze [--at RFC3339] [--config PATH]
bro eval snapshot get SHA256 [--config PATH]
bro eval status [--config PATH]
```

Generic eval sits outside the inference hot path. Routed requests create
redacted subjects automatically. A task-native runner, human, enterprise
system, or agentic evaluator submits the same immutable `EvaluationResult` via
CLI or authenticated REST. BitRouter validates evaluator authority and metric
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
These endpoints mutate evidence only; no evaluator can edit or publish a lock.

### `bro key sign`

```
bro key sign --user <id> [--db <url>] [--policy <policy-id>]
```

Mints a scoped `brvk_` virtual key for a user. The plaintext secret is printed once — only its SHA-256 hash is stored.

| Flag | Default | Description |
| --- | --- | --- |
| `--user` | *(required)* | Owning user id |
| `--db` | `sqlite://./bitrouter.db` | Database URL — `sqlite://`, `postgres://`, or `mysql://` |
| `--policy` | *(none)* | Policy id to bind to the key |

### `bro providers login <provider>`

```
bro providers login claude-code     # Claude Pro/Max subscription via Claude Code
bro providers login openai-codex    # ChatGPT subscription via Codex
bro providers login github-copilot  # GitHub device-code flow
bro providers login openai --api-key sk-…        # BYOK, non-interactive
printf %s "$KEY" | bro providers login anthropic --key-stdin
```

Runs the provider's OAuth flow (PKCE in a browser or device-code, depending on provider) and stores the token in `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. The slot is keyed by `(provider_id, label)` — pass `--label <name>` (defaults to `default`) to keep multiple accounts of the same provider side by side. Other providers fall back to a pasted API key.

For a provider that accepts a pasted key, `--api-key <KEY>` (or `--key-stdin`, which reads one line from stdin) seeds it non-interactively — skipping the method menu and the paste prompt. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For the built-in `bitrouter` provider the key seeds the cloud credential, exactly as `cloud login --api-key` does.

For `claude-code`, the login menu defaults to the live Claude Code session. For `openai-codex`, the default is **"Import an existing session from the vendor CLI"** — BitRouter reads the credential Codex already stored in `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS Keychain, and adopts it with no fresh browser sign-in. The imported token refreshes automatically like any other; choose the browser subscription flow when no local Codex session exists.

For cloud sign-in (signing into your BitRouter Cloud account, not an upstream LLM provider), see [`bro cloud login`](#bro-cloud-login--logout--whoami) below.

### `bro providers logout <provider>`

```
bro providers logout github-copilot
```

Removes every stored credential for the provider (subscription OAuth tokens and pasted API keys alike).

### `bro cloud login` / `logout` / `whoami`

Cloud sign-in, distinct from the per-provider `bro providers login` flow above. Interactive login uses the RFC 8628 OAuth Device Authorization Grant. For CI and other non-interactive environments, pass an existing BitRouter API key with `--api-key`. Both forms persist to the same credential file and are reused by `cloud api`, management commands, the built-in `bitrouter` provider, and account-attributed telemetry.

OAuth browser approval asks which workspace to bind; the resulting credential is **namespace-baked** (workspace-baked). To switch workspaces, re-run `bro cloud login`. OAuth credentials auto-refresh on use. API-key login performs no network request and management commands use the server's `me` namespace alias.

```
bro cloud login [--oauth-as <URL>] [--client-id <ID>] [--scope <SCOPE>]
bro cloud login --api-key <BRK_API_KEY> [--oauth-as <URL>]
bro cloud logout [--oauth-as <URL>] [--client-id <ID>]
bro cloud whoami
```

| Flag | Default | Description |
| --- | --- | --- |
| `--oauth-as` | `https://api.bitrouter.ai` (env: `BITROUTER_OAUTH_AS`) | Authorization server base URL — override only for a self-hosted deployment |
| `--client-id` | `bitrouter-cli` (env: `BITROUTER_OAUTH_CLIENT_ID`) | Public OAuth client id |
| `--scope` | broad developer set (env: `BITROUTER_OAUTH_SCOPE`) | Space-delimited scopes to request. Default includes `inference:invoke`, `usage:read`, `keys:read`/`write`, `billing:read`, `policy:read`/`write`, `byok:read`/`write`, `namespace:read`. Sensitive control-plane scopes such as `billing:write`, `user:write`, and `namespace:write` are opt-in. |
| `--api-key` | *(none)* | Store a `brk_<token_id>.<secret>` credential without browser login or network discovery. Conflicts with `--client-id` and `--scope`; intended for CI. |

Credentials are persisted at `<data-dir>/account-credentials.json` (mode `0600` on Unix). Existing untagged OAuth files remain compatible. `whoami` answers from the local file with no network call and reports `authentication: oauth|api_key` without printing a bearer. OAuth logout attempts RFC 7009 revocation before deleting the file; API-key logout is local-only.

---

## Workflow-state benchmark evidence

These commands export and validate the request-scoped evidence used by policy
benchmarks:

```text
bro workflow-state classifier-bakeoff --fixtures <DIR> [--submission <JSON>] --output <JSON>
bro workflow-state metering-usage --database-url <URL> --output <JSONL> [--since <RFC3339>] [--until <RFC3339>] [--impute-price <SPEC> ...]
bro workflow-state reconcile-metering --database-url <URL> [--api-base <URL>] [--api-key-env <NAME>] [--credentials-file <PATH>] --request-id <ID> ... [--price <SPEC> ...] [--max-attempts <N>] [--poll-interval-ms <MS>]
bro workflow-state reliability-report --database-url <URL> --config <PATH> --output <JSON>
bro workflow-state policy-oracle --traces <JSONL> --cloud-usage <JSONL> --policy-lock <YAML> --policy <NAME> --effective-cost-factor <0..1> --target-savings <0..1> ... --output <JSON>
bro workflow-state bundle --run-label <LABEL> --traces <JSONL> --cloud-usage <JSONL> [--outcomes <JSONL>] [--policy-decisions <JSONL>] --output-dir <DIR>
bro workflow-state apply-reward-feedback --database-url <URL> --traces <JSONL> --cloud-usage <JSONL> --outcomes <JSONL> --policy-decisions <JSONL>
```

`classifier-bakeoff` is a research-only, read-only route-context evaluation.
With no `--submission`, it records the compiled deterministic scorecard as an
uncalibrated baseline; its heuristic margin is never reported as probability.
An external submission must contain exactly one canonically ordered prediction
for every frozen fixture and bind the dataset, input projection, model artifact,
features, training split, and (when applicable) calibration split. Task family,
next-step role, progress, and shadow risk use separate heads. OOD and abstention
are explicit, and the shadow risk head is evidence only: deterministic rules and
signed policy remain authoritative.

The report contains per-head exact counts and macro-F1, per-slice coverage and
accepted error risk, fixed-point Brier/ECE for calibrated candidates, OOD
confusion counts, resource measurements when supplied, and a versioned
`classification_surrogate_loss`. This is a fixed label-error penalty, not
policy replay or measured routing cost/quality loss. Report and artifact schema
v2 replace the former `decision_weighted_loss` field, correct ECE to the
`0..1_000_000` ppm range, and emit `accepted_error_risk_ppm: null` when a slice
has no accepted predictions. Regenerate v1 reports from the original fixtures
and submissions; their ECE values cannot be repaired from the reported scalar.
Submission and manifest versions are unchanged.

The checked-in fixtures are an evaluation contract and
must not be used as both training and evaluation data; the command rejects
matching split commitments. Synthetic unit-test predictions are test vectors,
not classifier research results or production promotion evidence.

`reconcile-metering` reads the API-key environment named by `--api-key-env`
(default `BITROUTER_API_KEY`) first; a non-empty value takes precedence over
the optional owner-only BitRouter Cloud credential file. That file must contain
a static API key: OAuth credentials are rejected and are never refreshed for
settlement. Price specs use
`provider:model=uncached,cache_read,cache_write,output` in micro-USD per token.
Repeat the same provider/model pair when a gateway may have applied one of
several frozen schedules. A computed receipt is accepted only when exactly one
distinct candidate reconstructs its final micro-USD charge; no match or an
ambiguous rounding collision remains `unknown`.

`policy-oracle` performs an immutable cost-only replay of a candidate lock over
baseline traces and exact request settlement. `--effective-cost-factor` is the
candidate-to-baseline cost ratio after expected token, retry, and turn
inflation. The report includes cost-weighted route coverage, projected savings,
ranked eligible requests, the highest-cost routes still left on the default
tier, and the minimum covered requests needed for each repeated
`--target-savings`. It is an upper-bound prioritization report, not a quality
claim or a claim that the live trajectory will remain unchanged.

`bundle` is fail-closed: every non-empty trace set needs an exact request-ID
usage join and computed auditable charge; supplied policy decisions and outcomes
must each cover that same request-ID set exactly once. Omit `--outcomes` when
the terminal evidence is task- or episode-scoped and will be attributed through
the Eval Exchange. Omitting it records zero request-scoped outcomes; it never
broadcasts a task reward across the request set. Session/trial metadata and
timestamps are benchmark diagnostics, not strict join keys. Reward-feedback
admission also requires completed requests and authoritative settlement; it
does not use diagnostic identity fields for learning.

Bundles also write `routing-baselines.json` and embed the same report in
`run-artifact.json`. For each compatible candidate-set digest, the report
contains an always-tier control for every declared target and a deterministic,
content-blind control with exactly the observed selected-tier counts. Only
hashed decision identities are emitted. Baseline report schema v2 requires the
effective `(tier, model, effort)` to match a declared candidate. Continuation
pins to an undeclared effort are counted as `selected_target_mismatch`, as are
missing efforts when the candidate declares one; the pre-guard measurement is
not rewritten. Declared post-guard targets remain eligible, even when their
logging probability was zero. Dataset and baseline ID domains are versioned
to v2; regenerate controls from the original decisions. Legacy decisions
without measurement are counted as exclusions. These controls measure routing allocation; they do
not estimate the unexecuted models' quality or authorize a policy change.

---

## Policy

### `bro policy create <id>`

```
bro policy create strict [--dir ./policies]
```

Writes a starter policy file to the policy directory. Bind it to a key with:

```
bro key sign --user <id> --policy strict
```

---

## Cloud account management

`bro cloud …` drives the BitRouter Cloud API using the credential persisted by [`bro cloud login`](#bro-cloud-login--logout--whoami). Sign in first, then call a typed management subcommand or the generic API command. Typed subcommands cover the common terminal workflows: namespace inspection, API keys, usage and request history, billing balance and checkout, policies, budgets, presets, and BYOK. Use `bro cloud api <relative-endpoint>` for the rest of the Cloud API surface, including public provider and usage discovery, settlement receipts, routing presets, OAuth clients, billing ledgers, checkout status, and namespace/account lifecycle endpoints.

OAuth credentials are **namespace-baked** — keys, usage, and policies are scoped to the workspace chosen at login. API-key credentials use `/v1/namespaces/me/*`. The path segment is always resolved implicitly; callers never pass a workspace argument. `billing` and `byok` are user-level and reach across all workspaces regardless.

Every leaf accepts `--json` to print the raw response body instead of the human-readable summary. On a 403 whose description is `missing required scope: <s>`, OAuth users receive a copy-pasteable re-login hint that appends the missing scope; API-key users are told to mint or select a key with that scope and log in with it.

### `bro cloud api`

Make an authenticated request to any **relative** endpoint on the origin recorded by `cloud login`, modeled after [`gh api`](https://cli.github.com/manual/gh_api):

```bash
bro cloud api /v1/models
bro cloud api /v1/chat/completions --input request.json
bro cloud api /v1/responses -f model=openai/gpt-5 -F stream=true
```

```text
bro cloud api <ENDPOINT> [-X <METHOD>] [-H <KEY:VALUE>] \
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

This first release intentionally omits `gh api`'s GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname, preview, and placeholder expansion features. See the [Cloud API guide](/docs/guides/cloud-api) for copyable requests.

### `bro cloud whoami`

```
bro cloud whoami
```

Prints the cloud identity and the bound namespace alongside the `/v1/*` base URL the CLI will target. Reads the local credentials file only — no network call.

### `bro cloud namespace`

Inspect the workspaces you own and the one this CLI session is baked to. The typed CLI only inspects workspaces; creation and deletion require the Console or `bro cloud api` with the appropriate control-plane scope.

```
bro cloud namespace list    [--json]
bro cloud namespace current [--json]
```

`list` fetches all namespaces you own and marks the active one. `current` is offline — it reads the local credential and prints the bound namespace id without a network call. If the credential predates namespace-scoping, it prints `(no namespace — run \`bro cloud login\`)`.

### `bro cloud keys`

Manage `brk_` API keys in the active workspace. All minted keys are workspace-baked to the same namespace as the caller and cannot upscale their scopes beyond the caller's.

```
bro cloud keys list [--json]
bro cloud keys mint --name <NAME> --scope <SCOPE> [--scope <SCOPE> …] [--expires-at <RFC3339>] [--json]
bro cloud keys revoke <ID> [--json]
```

Requested scopes on `mint` must be a subset of your effective scopes (RFC 6749 §3.3 — no upscaling). The plaintext token is shown once in the `mint` response and is not recoverable after.

### `bro cloud usage` / `bro cloud requests`

Read aggregate spend / token counts and page through recent inference requests.

```
bro cloud usage    [--from <RFC3339>] [--to <RFC3339>] [--json]
bro cloud requests [--limit <N>] [--offset <N>] [--json]
```

`usage` defaults to a 30-day rolling window. `requests` clamps the page size to `[1, 100]` and defaults to 25.

### `bro cloud billing`

User-level — not workspace-scoped; reflects the account-wide wallet regardless of which workspace the CLI is signed in to.

```
bro cloud billing balance [--json]
bro cloud billing checkout --amount-cents <N> [--json]
```

`checkout` starts a Stripe credit-purchase session and prints the hosted URL. Requires the `billing:write` scope, which is opt-in — pass `--scope` to `bro cloud login` to request it.

Use `bro cloud api /v1/billing/transactions` for the billing ledger, and `/v1/billing/checkout/sessions/<session-id>/status` for checkout status.

### `bro cloud policy`

Generic CRUD over the typed policy registry (kinds: `budget`, `rate_limit`, `guardrail`, `preset`).

```
bro cloud policy list [--kind <KIND>] [--json]
bro cloud policy get <ID> [--json]
bro cloud policy create --name <NAME> --kind <KIND> --spec <FILE|-> [--json]
bro cloud policy update <ID> [--name <NAME>] [--spec <FILE|->] [--json]
bro cloud policy delete <ID> [--json]
bro cloud policy bind <ID> --principal-type <TYPE> --principal-id <ID> [--json]
bro cloud policy unbind <ID> <BINDING_ID> [--json]
bro cloud policy enable <ID> [--json]
bro cloud policy disable <ID> [--json]
bro cloud policy bindings <ID> [--json]
bro cloud policy effective --principal-type <TYPE> --principal-id <ID> [--json]
bro cloud policy for-principal <TYPE> <ID> [--json]
```

`--spec` reads the flat inner spec body as JSON from a file path or `-` for stdin. Principal types: `namespace`, `api_key`, `oauth_token`, `oauth_client`. `disable` parks a policy without deleting it — the engine skips disabled rows at request time.

### `bro cloud budget` / `bro cloud preset`

Typed wrappers over the budget-kind and preset-kind policy rows — same storage, flat wire shape (no `kind`/`spec` envelope).

```
bro cloud budget list [--json]
bro cloud budget get <ID> [--json]
bro cloud budget create --name <NAME> --window <day|month|total> --limit-micro-usd <N> [--json]
bro cloud budget update <ID> [--name <NAME>] [--window <W>] [--limit-micro-usd <N>] [--json]
bro cloud budget delete <ID> [--json]

bro cloud preset list [--json]
bro cloud preset get <ID> [--json]
bro cloud preset create --name <NAME> [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--json]
bro cloud preset update <ID> [--name <NAME>] [--guardrail <FILE|->] [--budget <FILE|->] [--rate-limit <FILE|->] [--clear-guardrail] [--clear-budget] [--clear-rate-limit] [--json]
bro cloud preset delete <ID> [--json]
```

Budget `--limit-micro-usd` must be strictly positive (the engine treats `<= 0` as "no policy" and the API refuses it up-front). Preset clauses are independently optional; use `--clear-*` flags to drop a clause from an existing preset.

### `bro cloud byok`

User-level — not workspace-scoped; BYOK provider keys are account-wide. The cloud only stores already-sealed ciphertext — seal against the cloud's current X25519 public key (separate fetch) before calling.

```
bro cloud byok list [--json]
bro cloud byok set    --provider <ID> --ciphertext-b64 <B64> --kek-id <ID> --key-prefix <PREFIX> [--api-base <URL>] [--json]
bro cloud byok delete <PROVIDER> [--json]
```

## Skills

`bro skills` reads and writes existing local skill directories. Installation and
remote updates belong to `npx skills`; BRO has no Git acquisition, installation
index, aliases or retained-version store. Existing old `.bitrouter/skills`
contents are left on disk and are not treated as directory discovery roots.

```text
bro skills list [--workspace PATH | --global | --root PATH]
bro skills show NAME_OR_DIRECTORY_ID [scope flags]
bro skills read NAME_OR_DIRECTORY_ID [--file RELATIVE_PATH] [scope flags]
bro skills write NAME_OR_DIRECTORY_ID [--file RELATIVE_PATH] --from FILE|- [scope flags]
bro skills init NAME [-o PATH]
bro skills explain --thread THREAD_ID --turn TURN_ID [--config PATH]
bro skills explain --managed-session NAME --request-id KEY [--config PATH]
```

Project discovery uses the selected workspace or current directory and its
conventional `skills`, `.agents/skills`, `.claude/skills` and `.codex/skills`
layouts. `--global` inspects user harness roots, including `CODEX_HOME` when set.
`--root` selects one existing directory. Scope flags conflict; duplicate names
require an exact directory identity or a narrower root. Invalid frontmatter stays
visible in listing. `read` defaults to `SKILL.md`; it returns bounded UTF-8 text.
`write` reads a file or stdin, validates root frontmatter, and atomically publishes
only the selected file; it does not execute scripts or register an installation.
Supporting-file parent directories must already exist. Both operations reject
paths outside the selected skill and symlink files; file text is bounded to
256 KiB. JSON is the default; `--human` is available.

Native `--skill project:ID` / `global:ID` identifies a discovered directory;
the ID is derived from its canonical path, not an installation UUID. Required
MCP identities remain `mcp:SERVER:URI`. Only explicit includes are selected;
`--exclude-skill` remains an explicit constraint. No decision backend is called
for skill selection, including supervised ACP. Native context preparation retains
complete admitted evidence and does not call a decision backend for omission,
summary/extract choice or task classification. SDK model routing remains separate.

A Turn freezes source identity and a file manifest in its original checkpoint.
This fingerprint is not a stored version. Later reads verify the directory's
actual bytes against it; edits or removal can reject continuation. A new Turn
can use the new contents. MCP skills keep their original origin, held manifest,
content consent, lazy reads and cache verification. Local directory writes do not
modify MCP resources or imply permission to use them. Native `skill_read` retains
its original Turn/caller/context bounds. ACP delivery is explicit local root
prompt instructions only: Codex text, or Claude negotiated embedded context.
It does not claim protected supporting-file access or MCP-origin delivery.

### `bro skills explain`

Read original native skill facts using `--thread THREAD_ID --turn TURN_ID`, or
`--managed-session NAME --request-id KEY` for daemon-owned managed inputs.
`--agent-turn ID` narrows either report to one exact child/root work unit.
These identities do not substitute for supervised ACP run IDs. An optional
`--database-url URL` selects the stored database directly and conflicts with
`--config`; relative SQLite addresses otherwise resolve against the config home.

The command reads original checkpoints, metadata, source/fingerprint/entry,
constraints, reasons and consents. Instruction evidence contains a digest and
byte count. Prepared model requests are reported only when their exact frozen SDK
input contains the bound instructions with the correct source and role. This
includes per-file digest/size and the requests containing its exact user data
frame, without returning file bodies. The human report lists the same file facts.
Preparation is distinct from provider execution: available attempt/HTTP outcome
facts are separate, and `attempts: null` means historical attempt details are no
longer in the current snapshot. It never implies that the model obeyed a skill.

Explain opens an existing database, without App/provider setup, daemon startup,
MCP discovery, file reads from the skill source, owner claims or recovery. SQLite
is forced to `mode=ro`, even if the supplied URL asks to create/write. Current
workspace/source files may be missing and unrelated provider/MCP config may be
invalid. Pending, failed, historical and unreleased inputs retain their original
evidence; inspecting a lost owner does not authorize its continuation. Only the
selected database is queried. Named remote-control contexts are rejected rather
than falling back to local history. JSON and `--human` are supported.

### `bro skills init <name>`

```
bro skills init <NAME> [-o|--output <PATH>]
```

Scaffolds a starter `<NAME>/SKILL.md`; `--output` may choose another path whose
file is still named `SKILL.md` and whose parent directory equals `<NAME>`.
Refuses to overwrite an existing file. Names follow the Agent Skills grammar:
1–64 Unicode lowercase alphanumeric characters, or non-leading/trailing single hyphens.

## Local ACP recordings

Opt in with `acp_recording.enabled: true` in the selected config. This records
observable ACP content from external `code <agent>`, `run`, and `acp serve <agent>` in the local database,
independently of `trajectory.enabled`. It does not invoke an evaluator.

```bash
bro acp recordings [--config PATH] list --agent codex-acp
bro acp recordings [--config PATH] show --agent codex-acp NATIVE_SESSION_ID
bro acp recordings [--config PATH] delete --agent codex-acp NATIVE_SESSION_ID
```

The source is the resolved configured agent ID, and the session ID remains
harness-native. `show` returns canonical events, separate replay audit, gaps,
and observed metering/route links. Output follows the global JSON/`--human`
convention. Content persists until explicit deletion; deletion fences further
recording for that native identity and does not touch native harness history
or metering. See [the recording contract](ACP_CANONICAL_CAPTURE_SPEC.md) for
visibility, write-failure, and cost-accounting semantics.

## ACP checkpoints and assessment history

Use the source and native ID from `acp recordings list`, and the current `head`
from `acp recordings show`. Creation rejects a stale expected watermark.

```bash
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] create --watermark N
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] list
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] show CHECKPOINT_ID
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] resources CHECKPOINT_ID [--refresh]
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] submit assessment.json
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] history
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] effective
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] family
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] rubric prepare CHECKPOINT_ID
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] rubric submit rubric.json
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] judge CHECKPOINT_ID --model MODEL
bro acp checkpoints --agent SOURCE NATIVE_ID [--config PATH] judge-job JOB_ID [--resume]
```

Their evidence comes from existing local records only. `judge` explicitly sends
the frozen evidence to the selected configured model. `create` freezes event references
and observes local resource records. `show` verifies and resolves original source
versions. `resources` lists immutable metering observations; `--refresh` adds an
observation if local data changed, without changing the content checkpoint.

Local recording registers request-inventory coverage with the serving daemon
before forwarding its first event. An older/unavailable daemon or a remote
endpoint leaves this coverage unacknowledged; recording itself remains usable.
The resource output includes `gateway_coverage` with its scope, revision fences
and incompleteness reasons. `metering_complete` applies only to BitRouter-managed
model requests and requires closed prompt boundaries, healthy acknowledged
capture, resolved request membership and complete settled attempt costs.
Unknown prices, unfinished requests and incompletely priced fallbacks remain
unknown. Refresh after late settlement to obtain a new immutable resource
observation. A known subtotal alone cannot authorize evolution promotion.

`submit` imports a JSON revision envelope. Copy checkpoint and current revision
IDs from command output. The pipeline and selection digests identify the scoring
configuration that produced the imported labels; this layer does not generate
templates or scores. For example (replace identifiers and digest placeholders):

```json
{
  "submission_id": "human-review-1",
  "checkpoint_id": "CHECKPOINT_ID",
  "expected_revision": null,
  "source": "human",
  "evaluator_id": "local-user",
  "evaluator_version": "1",
  "assessment": {
    "pipeline_config_digest": "LOWERCASE_SHA256_DIGEST",
    "selection_digest": "LOWERCASE_SHA256_DIGEST",
    "scores": {
      "completion": { "status": "scored", "value_ppm": 1000000 },
      "correctness": { "status": "unknown" },
      "pr_delivery": { "status": "not_applicable" }
    },
    "evidence": [],
    "explanation": "Judgment based on the recorded checkpoint."
  },
  "reason": "Initial manual assessment"
}
```

Use `expected_revision: null` only when no selection exists. For a correction,
provide the current revision ID and a new submission ID. A retraction uses
`assessment: null`, the current checkpoint/revision IDs, Human source and a
reason. Evidence entries have `node_id` and `digest`, matching checkpoint
references. Scores range from 0 to 1,000,000 ppm; unknown is not zero.

Retries with the same submission ID and input are idempotent. Older workers
cannot overwrite a newer selection. Appending a session marks its previous
assessment stale, while preserving the old checkpoint. Deletion invalidates
dependent checkpoints and removes associated assessment text. Only `judge` and
`judge-job --resume` invoke a model; checkpoint and manual scoring commands do
not publish routes. See [the checkpoint contract](ACP_CHECKPOINT_SPEC.md).

`judge` creates a durable job for the checkpoint/model contract. It uses the
fixed rubric library, sends no tools or ACP routing identity, and imposes no
product token or cost ceiling. A completed retry returns the same job without
another model call. `judge-job` inspects its status, request IDs, cached output
and selected revision; `--resume` retries the recorded input/model after its
worker lease expires. A cached valid response is reused after a restart.
Uncertain attempts retain distinct request IDs for cost accounting. There are
at most three model attempts per job; this operational retry limit is not an
evaluation budget. Failed jobs are inspectable and cannot overwrite a newer
manual revision. Deleting source content removes cached judge text as well.

`rubric prepare` exports the fixed coding rubric library, the current revision
ID, and original cited evidence from the selected checkpoint. Evidence carries
`projection_version: "recorded-acp-quality-evidence-v2"`. Protocol routing/usage
metadata, provider setup and private thought chunks are omitted, including nested
session-result metadata. Stop reasons and error code/messages are retained; the
canonical source and citation digests remain unchanged. Task/tool content is
preserved and may still reveal identity clues. The output contains conversation
content and must be treated as a local content export.

`rubric submit` accepts `submission_id`, `checkpoint_id`, `expected_revision`,
`source`, `evaluator_id`, `evaluator_version`, and `evaluation`. The evaluation
contains `rubric_version: "coding-checkpoint-rubric-v2"`, `items`, `diagnostics`,
`severe_violation`, `violation_evidence`, and `summary`. Every library item must
appear once, with `criterion_id`, `applicability` (`applicable`, `not_applicable`,
or `unknown`), `selection_reason`, `score`, `evidence`, and `explanation`. Scores
and citations use the checkpoint forms described above. Diagnostics carry
`criterion_id`, an evidence-backed `role` (`introduced`, `discovered`, `repaired`,
`inherited`, or `unknown`), `evidence`, and `explanation`.

Mandatory rubrics cannot be excluded; unknown applicability remains unknown;
positive verification needs a tool observation, not just an assistant claim.
Weights are fixed by the library. Returned lower and upper quality values bound
missing rubric values, not statistical confidence. Programmatic validation checks
the contract and references; it does not prove that every semantic judgment is
correct. Manual submission makes no model call. A model acting as a reference
reviewer must use `source: "agentic"`, not `human`.

Rubric v2 binds responsibility to the recorded task: review-only findings are
scored under delivery, and `review_resolution` applies only to an obligation to
resolve findings or obtain acceptance. Explicitly forbidden/deferred execution
is excluded from verification for that checkpoint. Required checks blocked by
the environment remain applicable with unknown scores; they are not automatically
code failures. Positive executable verification needs relevant execution evidence,
not a source-file read. Constraints describe how work was performed and do not
count a missing deliverable a second time.

The judge is `recorded-evidence-judge-v2`. Rubric and evidence versions are included
in measurement contracts. Historical v1 labels remain inspectable but are not
converted or pooled into v2 learning. Incomplete jobs whose input contract changed
are retired before reserving another request. A committed historical revision
remains intact even if its old job was not finalized. Explicitly judging again
creates the current-version job; this upgrade does not bulk-judge history.

### `bro acp evolution`

Control checkpoint feedback and policy-block experiments through the existing
local serving daemon. Use the same config as that daemon; these commands do not
start it automatically.

```bash
bro acp evolution [--config PATH] status
bro acp evolution [--config PATH] mode off
bro acp evolution [--config PATH] mode manual
bro acp evolution [--config PATH] mode automatic --judge-model MODEL
bro acp evolution [--config PATH] register block.json
bro acp evolution [--config PATH] revise next-block.json --expected-experiment EXPERIMENT_ID
bro acp evolution [--config PATH] restore BLOCK_ID --expected-experiment EXPERIMENT_ID --expected-revision REVISION --reason "Reason for withdrawal"
bro acp evolution [--config PATH] learning BLOCK_ID [--experiment EXPERIMENT_ID]
bro acp evolution [--config PATH] improve BLOCK_ID [--experiment EXPERIMENT_ID]
```

Evolution defaults to `off`. `manual` discovers stopped recorded checkpoints
without invoking a model; submit their scores with `acp checkpoints ... rubric
submit`. `automatic` also judges those checkpoints using the configured model.
The model is retained across mode changes, so `--judge-model` may be omitted
after one is saved. Changing it starts a new feedback epoch and fences older
automatic jobs. There is no product token or cost ceiling for the judge.

The daemon polls recorded prompt stops in the background. It considers stops
after the current mode/model epoch began, so enabling automatic mode does not
bulk-judge historical sessions. Continued sessions produce later immutable
checkpoints and replace their effective learning contribution. Explicit `judge`
commands can evaluate older checkpoints. Recording must be enabled separately.

Resource membership `native-head-resources-v2` includes this session's model
requests admitted through its checkpoint head, including auxiliary calls after
a prompt stops at that same head. They update cost without another content
evaluation. A new prompt advances the head; inherited fork prefixes still exclude
later parent work. Unresolved or unfinished calls remain incomplete. Older
resource records are preserved and refreshed before learning uses their costs.
Complete cost describes the observed inventory; final cost through worker exit
also requires confirmed capture closure and settled request outcomes.

Trial assignment requires a new recorded session. ACP setup notifications about
commands, configuration, mode or session metadata preserve that eligibility.
Earlier assistant content, tools, usage or unknown/malformed updates prevent
late enrollment. A worker warning emitted as assistant text also takes this
conservative path; a custom model alias that triggers such a warning can remain
on its baseline for the session. The enrollment's `admission_reason` explains
this exclusion, and later requests do not resample it.

`status` reports the control state, worker progress, scheduled checkpoints and
judge job summaries, including failed attempts. Cached model text is available
through the session-scoped `judge-job` command rather than this summary. A
restart resumes eligible durable work; shutdown preserves uncertain attempt IDs.
Queued work made obsolete by an append, manual correction or mode change is
marked `superseded`. Completed canonical assessments can have their job receipts
recovered while off, without a model call or a new assessment selection.

`register` imports a complete `BlockDefinition` JSON object and checks its policy
selectors and dependencies against the live serving configuration. Registration
does not change the feedback mode. `learning` reports evidence and proposed
allocation without publishing. `improve` may publish an eligible allocation,
adoption or withdrawal; the background worker performs the same reconciliation
while evolution is enabled. Both paths require current evidence and live route
dependencies. Turning evolution off fences new automatic judging, trial
assignment and publication, while retaining adopted baselines.

`revise` starts the next experiment for an existing block. Read its current
`experiment_id` from `status` and pass it as `--expected-experiment`. The new
definition must preserve the block ID, agent source and complete matcher set.
Its baseline routes must inherit the currently supported baseline: the previous
candidate if adopted, otherwise the previous baseline. If the previous live
route contract or declared dependency changed, use the configured selectors as
baselines. Routes and the predecessor are rechecked before registration.
Registration preserves the mode and starts fresh learner/cohort state.

Prior versions remain under `archived_experiments`; existing sessions retain
their version and arm. `--experiment` targets a specific current or archived
version, while omission selects the current version. Archived trials accept no
new sessions and cannot gain a new adoption. Their feedback can still retract
an adoption and withdraw later versions that inherited it. Routing falls back
to the last supported baseline, subject to live dependency checks. Other blocks
are unchanged except where they declare dependencies on a withdrawn revision.

Use the CLI `candidate` and `revise` operations to preview and register an
experiment. Retain the inspected predecessor and request ID across retries.
`learning --experiment` inspects a specific archived version; reconciliation
remains bound to that version after later revisions are registered.

`learning` distinguishes the original trial evidence from quality monitoring of
new sessions after adoption. `improve` and the worker can withdraw an adopted
block when valid monitoring evidence crosses its quality floor or records a
severe violation. Continued monitoring does not add randomized trial samples or
prove ongoing comparative cost savings. Original trial corrections can also
invalidate adoption. Sessions first admitted while off are not enrolled for
monitoring retroactively.

During cold start, learner v2 retains bounded initial allocation until both arms
have the configured minimum independent quality, cost and latency evidence.
Pending/cumulative trial limits and quality withdrawal still apply. Existing
reduced exposure is not automatically raised. A persisted incompatible learner
plan holds new trial admission until local reconciliation; this does not require
another judge call or reassign existing sessions.

See [the evolution contract](ACP_EVOLUTION_SPEC.md) for the block, reward and
publication semantics. Use the CLI controls above for mode, rubric and existing
block management. These controls do not establish that automatic promotion is
safe on real coding tasks.

## Request-check extensions

There is no `bro checks` command or request-check receipt API. Validate native
declarations with `bro config validate`; custom-host activation additionally
verifies compiled registrations and revisions. Checker declarations and router
bindings require restart after changes. Startup logs and bounded invocation
tracing provide content-free diagnostics; absence of telemetry is not proof that
a check did not run. Existing `bro requests` remains the settled cost/usage
interface. See [REQUEST_CHECKS_SPEC.md](REQUEST_CHECKS_SPEC.md) for the execution
and coverage contract.

### Native BRO resource configuration

Native coding Turns use `mcp_servers` from the daemon configuration through the
orchestrator's direct MCP client. `mcp check` uses the same connection/discovery
implementation and honors `mcp.upstream_protocol`. MCP calls follow the existing
identified approval policy; `--read-only` does not connect to MCP. Configuration
is fixed at daemon startup: restart to replace native MCP transport/credentials.
Skills discovery supplies metadata and versions without prompt injection.
See [BRO harness resources](BRO_HARNESS_RESOURCES.md).

`bro workflow-state reliability-report --database-url <URL> --config <PATH> --policy <NAME> --output <PATH>` reads reliability thresholds from the named policy in the version 4 lock (`--policy` defaults to `auto`).


## Offline DB evidence (Unix SQLite only)

`bro tenancy offline` operates on explicitly supplied local paths. It does not
load host configuration, discover a daemon, contact HTTP, migrate a source, or
bootstrap an installation. Stop all source writers before capture. The existing
canonical exclusive DB ownership lock refuses participating live writers,
including symlink aliases; hard-linked DB files are rejected. Old/external
writers that do not participate in this lock still require an external fence.

```sh
bro tenancy offline snapshot --database /absolute/state/bitrouter.db --bundle /absolute/new-db-evidence
bro tenancy offline inventory --bundle /absolute/new-db-evidence
bro tenancy offline verify --bundle /absolute/new-db-evidence --evidence-digest <digest-from-snapshot>
```

The bundle path must not exist. Preserve the returned `evidence_digest` outside
the bundle and pass its exact64 lowercase hexadecimal characters to `verify`.
Inventory recomputes the actual contents but does not authenticate the origin of
a self-contained bundle. Verification compares the supplied evidence anchor,
private profile, file digest and actual typed SQL inventory. Neither operation
grants execution, migration, owner assignment or recovery authority.

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

`bro tenancy offline audit --archive DIRECTORY --projection NEW_DIRECTORY --evidence-digest SHA256 --control-config FILE --listen 127.0.0.1:PORT` serves only the existing authenticated receipt GET/page routes for the archive's original installation. The explicit control file is bounded JSON with a `credentials` array of existing `id`, `token_env` and `scopes` entries; each requires `control:tenancy`. Bearer values are read only from those named environment variables, never command arguments. Example:

```json
{"credentials":[{"id":"archive-auditor","token_env":"ARCHIVE_CONTROL_TOKEN","scopes":["control:tenancy"]}]}
```

Use a private mode0600 control file and a mode0700 verified M4a bundle. The projection must be a new directory outside that bundle; its verified DB copy and canonical ownership sidecar do not alter the archive. Only Unix local SQLite with the exact supported current migration ledger and bounded schema/inventory is accepted. No source migration, missing-identity initialization, configuration loading, maintenance, credential refresh, provider call or command route runs. `--context`, endpoint/token flags and ambient host configuration are not audit inputs. The listener must be loopback; HTTP/2 follows the existing optional `http2` feature.

Every receipt request supplies `Authorization: Bearer ...` and the mandatory old `Bitrouter-Installation-Id`. Responses use `Cache-Control: no-store`. The first profile authenticates only the explicitly configured installation-operator control credentials; archived tenant ingress tokens are not audit credentials. Unknown receipt IDs are 404; wrong installation is a typed mismatch. Other management, inference, capability discovery and mutation routes are absent.

The running command emits one nonsecret readiness report containing the bound address, old installation and evidence digest. SIGINT/SIGTERM stops admission and retains each accepted read until its actual rollback and worker close are acknowledged. A dropped HTTP/CLI waiter cannot release the SQL owner. Unconfirmed bootstrap/close retains the canonical lease until process exit; elapsed time is not a cleanup witness. Limits: 32 sockets with a 60-second maximum connection lifetime, eight reads including queued/reply-held work, five-second read budget, up to 100 IDs/page and 8 MiB encoded response bytes. Reply buffers retain their read slot through transport clones and slices. Preparation observation is bounded to 120 seconds; cleanup observation is bounded to 30 seconds without aborting the owner.

The bundle remains `sqlite_database_only`, `complete=false`, `cutover_ready=false`. This command is receipt audit, not recovery verification, new-installation provisioning or a full backup. External credentials/workspace state and unknown upstream effects remain unassessed. Operators must maintain retirement of the source writers; a new sidecar does not fence arbitrary old binaries. Requalify corrected M4a and the final archive consumer before shipping this profile.

#### Offline mapping review

`bro tenancy offline map --bundle DIRECTORY --evidence-digest SHA256 --manifest FILE --report NEW_DIRECTORY` validates the frozen M0 MigrationManifest and binds its source snapshot to a verified DB-only bundle. Inputs must be private regular files; no named context, host config or network lookup is used. This local profile accepts at most 8 MiB, 4096 mappings, 4096 merge approvals and 16 examples per mapping. Each declared disposition total must fit an exact JSON integer.

The new secured report directory records the original manifest and nonsecret validation summary. Duplicate mapping keys, missing or extraneous merge approvals, unresolved owners with grants, foreign observed installations and wrong snapshot digests fail. Same text in different owner namespaces remains distinct. The RECORDED marker only seals this review packet; it is never bundle READY or an execution capability.

Counts and evidence inside the manifest remain operator claims. The command does **not** authenticate external ownership/producer evidence, count per-owner source records, backfill rows, provision a target or retire writers. Missing installation identity remains explicitly unobserved. All completion/cutover flags stay false. Historical family codecs, explicit quarantine/backfill and operational cutover/rollback verification are still required.

### Bounded Legacy Eval import (Core, Unix SQLite)

`bro tenancy offline eval-inventory --bundle DIRECTORY --evidence-digest SHA256 --source-kind local_user --report NEW_DIRECTORY` validates the four-table Legacy Eval graph and records exact keyed owner partitions. `shared_user` and `native_owner` are explicit alternative source namespaces, never inferred from owner names. Wildcard snapshots are unresolved quarantine.

`bro tenancy offline eval-backfill --plan FILE --plan-digest SHA256 --report NEW_DIRECTORY` consumes an independently anchored private `bitrouter.offline.eval-backfill.v1` plan. It requires a stopped, freshly Service-provisioned destination with migration038 already applied, actual current tenant-wide admin bindings, and exact fresh provisioning history. It performs no provisioning, schema migration or counter reset. Inputs use explicit absolute paths; there is no context lookup or network call. See [the private-format profile](RESOURCE_TENANCY_EVAL_IMPORT.md) for plan fields and the required operator trust boundary.

The complete source/target inventory is limited to 4096 rows, 32 MiB typed values and 1 MiB per cell; the transformed target must also fit those bounds. An 8 MiB M0 mapping manifest must match every Eval partition. Only selected Eval rows are inserted into an empty destination graph. All other source tables and unresolved/quarantined Eval partitions remain in external immutable evidence. Snapshot IDs, original root and manifest bytes are preserved through exact private versioned provenance; ordinary owned readers require that record to validate an imported Legacy root. No tenant authority is derived from source metadata.

Success publishes a secured `RECORDED` packet only after explicit COMMIT, exact full-content verification and actual SQLite worker close. It remains `complete=false` and `cutover_ready=false`. A failed/abandoned reply may follow a committed import; retain any `INCOMPLETE` packet and keep the destination stopped for explicit reconciliation. Do not retry under another plan or merge history. Unknown SQL outcome/cleanup retains the canonical file fence until process exit. External writer retirement, target restart/readback, ingress release and compatible rollback are separate M4 gates. Production dispatch and strict-receipt flags are unchanged.

### Fresh installation recovery verification (Unix SQLite)

`bro tenancy offline recover --plan FILE --report NEW_DIRECTORY` verifies an
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
