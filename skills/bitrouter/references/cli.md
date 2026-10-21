# CLI reference

## Single-operation native and ACP controls

Use the running local daemon; queries do not start it. `--config PATH` selects
its source, `--socket PATH` overrides the control socket. Native
`--server-instance` pins the native runtime; ACP/evolution pins the control boot.

- Native: `bro threads list/create/show/history/events/start/enqueue/steer/pending/answer/cancel/resume/close/unload/load/recover`. Create uses `--workspace PATH --model SELECTOR` and Ask by default; `--read-only` conflicts with `--check`. Start/enqueue use `THREAD --prompt-file PATH` (`-` reads stdin once). Steer also requires `--turn TURN`; answer uses `THREAD --turn TURN --request INPUT allow|deny`; cancel adds `--queued` only for a queued Turn.
- Mutations accept `--request-id KEY`. Retain key, target, payload and runtime after uncertain ACK; do not change identity to repeat work. Pending is read-only. Tool and verification approvals stay separate. Queue resume is not execution recovery. Close cancels/joins and empties the queue, preserving the paused reusable Thread/history. Explicit load uses `--workspace`; recover reads an exact `ThreadRecoveryRequest` through `--from FILE`. Unknown effects stay blocked.
- Supervised ACP: `bro agents show/events/pending/prompt/answer/cancel/review RUN`. Prompt reads `--prompt-file`; answer requires exact `--request PERMISSION --option OFFERED_ID`. `agents mode show/set`, `config show/set --value-json JSON`, `route show/set/reset`, and `session list/new/load/resume` retain run/native-session scope and capabilities. Config JSON accepts string, boolean or typed ACP value object. A route bridge is required; global preview is not a substitute.
- ACP mutations use transient leases without takeover. Reports retain request/client/generation/boot. Exact ACK retries require original `--request-id`, `--client-id`, `--lease-generation`, and `--server-instance`; `agents action RUN` inspects that ACK without replay. Supervisor v3 adds background skill constraints and retains v1/v2 ordinary operations; v2 added ACK inspection. Missing boot-local ACK means unknown. Existing `agents stop/remove` stay separate process/metadata operations; harness-owned sessions remain.
- `events --follow` appends NDJSON; Ctrl-C only ends observation. Native events distinguish snapshot/catchup/resynchronization/live output. ACP reports bounded replay, gap and `history_complete`; retain cursors and do not call it durable history. Reports and `--human` preserve the same facts without terminal controls.
- `bro acp evolution candidate catalog/revision/preview/register` uses typed spec/preview JSON and exact generation fences; `judge-model MODEL` changes no mode. `checkpoint --source SOURCE --session NATIVE_SESSION list/freeze/review/submit` uses frozen evidence, expected watermark/revision and exact submission IDs. History inspection changes no reward. Existing publication/restore constraints remain.

Native local v19, HTTP agent v2 and native ACP v1/v2 are separate contracts.
ACP control v3 confirms its boot after grant issuance and never refreshes the
grant on a new boot. Evolution local control negotiates v1 and checks the boot
at the mutation owner; no fallback to the legacy unpinned operation is allowed.
Foreground ACP denies unmatched requests; background ACP still Ask. Raw ACP
auth/fork/extensions stay capability-gated. These controls are single operations; first-party UI and conversation input loops
are removed.

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
`--max-output-tokens`, `--request-id`, `--server-instance`, `--skill` and
`--exclude-skill`. The last two repeat exact selectors: `project:INSTALLATION_ID`,
`global:INSTALLATION_ID`, or `mcp:SERVER:URI`; e.g.
`--skill mcp:reviews:skill://review/SKILL.md`. `task run` accepts the same selectors
and retains them in the created Thread config; managed retains them in its input
identity. Inclusion is required, exclusion prevents selection, and conflicting
selectors fail before submit. Clients require advertised `skill_constraints`;
an older daemon cannot silently ignore them. An omitted model
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
`managed` emits `managed_skill_consent` and waits. Inspect `managed-status`, then
use `bro task managed-answer --session NAME --request-id KEY --server-instance BOOT --consent-id ID --decision allow|deny`.
It supports `--config`/`--socket` and binds the original input, agent Turn, owner
epoch, complete entry and permission state. Identical retries return the original
`answer_receipt`; a cold journal cannot accept new consent or restore an owner.
`task run` also waits on `mcp_skill.activate`; answer through `threads pending`
and `threads answer`. MCP bodies enter source-tagged user data, with complete held
entries, and do not automatically pass to unconsenting child tasks. Approval
does not expand tool grants. Original MCP root/file caches remain in the Turn and are reverified before reuse.
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

Standard output is NDJSON: `accepted` includes `thread_id` and `turn_id`,
`snapshot` carries a Thread view, durable `event` records use Thread cursors,
`live` carries bounded volatile output, and `terminal` includes status,
verification, final answer, and cursor. Failed, cancelled, interrupted, or
recovery-blocked Turns exit nonzero. This local headless client approves its
own identified tool requests. `--check` supplies bounded workspace verification;
without it verification is `not_requested`. `--read-only` forbids effectful
tools and cannot be combined with `--check`. Explicit remote contexts fail
without local fallback. `bro run <agent>` remains the separate ACP harness path.

The local native protocol is **v19**, bound to the negotiated server instance.
Older clients/daemons fail before submission or approval, preventing v18 task
clients from auto-allowing MCP skill requests. HTTP v2 advertises
`explicit_skill_consent: true` and requires `skill_consent: true` in `/inputs`
for `mcp_skill.activate`. Ordinary command policy grants no such consent.
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
or prompts. New/updated roots use runtime format 5; formats 2/3/4 remain readable without history rewriting. Migration adds metadata with
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

Every subcommand the v1 binary actually exposes. Anything not listed here doesn't exist — don't suggest `bro doctor`, `bro providers add`, `bro cloud connect`, or the old auth subcommand tree (cloud identity is `bro cloud whoami`, see below).

Bare `bro` ensures the selected local daemon is running in the background, waits
for its control-socket readiness response, emits one JSON report (`--human` is
also available), and returns to the shell. Repeated or concurrent invocations
reuse that daemon. A source-verified running target also remains reusable when the client no longer has its startup credentials or YAML. A valid Router configuration or detected zero-config credentials
is sufficient; `chat.agent` is optional. The bare result has `action: ensure_start`,
`status: ready`, `pid`, `listen`, `models`, `log`, and optional `config_state`.
Detection does not verify provider login
or connectivity. Explicit remote contexts require an administration subcommand.

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
OAuth redirects have echo disabled. EOF/Ctrl-C cancels and restores input state.
Router settings are saved before optional resources; cancellation there preserves
that progress and skips Finish. Prompts and diagnostics go to stderr.


Optional resources reuse the existing MCP publisher and versioned installer.
The menu supports Add MCP server, Install skill, retry and Continue. Completed
Router/resource writes survive failures. Headless flags are paired `--mcp-name
NAME --mcp-from FILE|-`, plus `--install-skill SOURCE`, optional `--skill
NAME_OR_PATH`, `--skill-ref REF`, and `--workspace PATH` or `--global`. In the
wizard stdin belongs to prompts, so MCP input must use a file. `--yes` never
prompts to resolve multi-skill ambiguity, even on a TTY. The `resources` array
retains each safe receipt or failed/skipped outcome. Headless resource failure
emits one partial report with nonzero exit and `after: exit`, preserving Router
settings and suppressing following setup/start/launch actions. Inspect saved
state before retrying; use `mcp edit`/`skills update` for existing names. These
steps do not connect MCP, grant skill-source permission, execute scripts or
activate skills. Explicit Finish may start the configured daemon afterward.

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

Reports retain `harnesses_installed: []` as a deprecated field and add
`agents_selected` for selection only. After serve, `daemon` carries readiness
and `config_state`. All saves report `config_activation: saved_only`.

Global `--context NAME` selects a named remote-control target for `status`
(including `--requests`), `requests`, `models`, `route`, `providers list`,
`observe status`, `policy status|show`, and `agents list`; `local` forces normal local behavior. `reload` is the
one remote mutation, and `operations show <request-id> --instance <boot-id>`
looks up its retained receipt.
Manage targets with `bro context add NAME --endpoint URL --token-env
ENV`, `context list`, `context show NAME`, and `context remove NAME`. The store
contains the token environment-variable name, never its value. HTTPS is
required except for loopback/SSH-forwarded HTTP. The client handshakes first,
follows no redirects, never falls back to local state on a remote error, and
rejects local `--config`/`--socket` before reading local config, environment, or
metering data. Remote `agents list` is catalog-only: external-registry fetches,
checks, launches, and ACP sessions remain local. Remote policy defaults to
`--view active`; local policy defaults to `--view disk`.

## Daemon lifecycle

| Command | Effect |
|---|---|
| `bro serve [--config PATH]` | Run the inference HTTP server + local control socket **in the foreground**. Optional `control.enabled: true` also starts the authenticated typed HTTP control listener at `control.listen` (default `127.0.0.1:4358`). `control.credentials` may name `{id, token_env, scopes}` credentials using `control:read` and `control:reload`; absent/empty credentials preserve `BITROUTER_CONTROL_TOKEN` as read-only. Explicit credentials exclude that legacy token. Tokens are at least 32 bytes, browser origins are checked, the listener stays loopback-only for a private tunnel/TLS reverse proxy, and inference `server.skip_auth` does not affect it. This optional HTTP control listener exposes neither an MCP origin nor remote ACP sessions; native ACP uses its separate OS-local stream. |
| `bro start [--config PATH] [--log PATH]` | Spawn `serve` as a detached background process. Stdout/stderr go to `~/.bitrouter/bitrouter.log` unless `--log` overrides. Refuses to start over a live daemon. |
| `bro stop [--config PATH] [--socket PATH]` | Graceful shutdown via the control socket. |
| `bro restart [--config PATH] [--log PATH] [--socket PATH]` | Explicit restart: for file SQLite, preflight migrations on a snapshot and retain a recovery backup, then request graceful stop and start. Warns that active agent runs may be interrupted. Does not force-kill a process that fails to exit. |
| `bro update [--check] [--tag TAG] [--stable] [-y]` | Self-managed updates attempt a safe daemon handoff by default after replacing the binary; `--restart` is a hidden compatibility flag. Homebrew, Cargo, and npm installs delegate to their package managers. A busy or incompatible daemon is left running and reported as `deferred`. |
| `bro reload [--config PATH] [--socket PATH]` | Hot-reload the running daemon's config + routing table. **Also re-pushes provider env vars** from the current shell into the daemon, so `export OPENAI_API_KEY=new...; bro reload` rotates the key without a restart. `bro --context NAME reload` instead reloads server-owned files without client env forwarding; non-success, running, partial, or unknown results exit non-zero and include a retained operation lookup. SIGHUP reloads daemon-side config but cannot forward newly exported shell variables. All three entrypoints reject startup-only changes before application; inspect `status.config_state` to choose reload or restart. |
| `bro status [--config PATH] [--socket PATH]` | Passive status: pid / listen / model count / providers / socket, installed and daemon versions when available, and `spend`. Reports `stopped` (exit 0) when no daemon is reachable — and still reports spend, which is on disk and outlives the daemon. **`spend` is two independent facts**: `spend.spent` (money already gone — `estimated_micro_usd` over `window`, `requests`, `unpriced`) comes from the local metering database on *any* deployment; `spend.limit` (money left — `balance_micro_usd`, `pending_micro_usd`, `remaining_micro_usd`) appears only where a cap exists, today a metered cloud account's prepaid credit. `spent` is an **estimate and a floor**: requests with no charge evidence are excluded rather than summed as zero, and `unpriced` counts them — non-zero means the figure understates by an unknown amount, so never read it as a total. A `limit` is the opposite, an authoritative ledger. No `spend` key at all means no metering database was readable, which is *not* the same as `estimated_micro_usd: 0`. Machine-wide, not per-caller. |
| `bro requests [--limit N] [--since RFC3339 --until RFC3339] [--model ID] [--provider ID]` | Newest-first settled requests (time, model, provider actually used, tokens, cost, latency, status) + the filtered-window spend and host-wide trailing-minute rate. **JSON by default**; `--human` renders the table. The time pair is required together and spans at most seven days; `limit` is 1–500. Reads the metering store directly and works with no daemon (`mode: history_only`). Portable to named remote contexts. `status --requests` is a hidden compatibility spelling. |

Configuration status uses the target-owned optional `config_state`: saved input availability, running `in_sync/reload_required/restart_required/mixed/unknown`, safe changed-field categories, and separate policy-lock/access-policy evidence. Missing evidence from an older daemon stays unknown. Code inspectors consume this same report. Live responses never combine a client's alternate config with the daemon's source. A source-specific local locator preserves access to the running endpoint after socket edits or invalid/deleted YAML, and validates its daemon instance; `--socket` overrides it. `init` and `policy init` report `config_activation: saved_only`, not runtime activation.

The launcher passes the hidden `serve --managed-child` flag only to its own
detached child. Automatic handoff requires that ownership marker, an idle
daemon, and SQLite migration preflight. A foreground `bro serve` stays under
its external supervisor's control.

Standalone BRO runtime daemons defer automatic replacement for the native
endpoint's lifetime; ACP/HTTP idleness cannot prove native queue/worker
quiescence. Finish native work and use explicit `bro restart` to change binaries.

## Inspection

| Command | Effect |
|---|---|
| `bro route <model> [--prompt TEXT] [--config PATH]` | Preview a model or router using the running daemon, otherwise local config (`resolved_via: live/config/zero_config`). The config fallback discovers models for `auto_discover: true` providers, as `bro models` does. Fixed routes show `effective_model`, `provider_chain`, and available rate estimates. Policy-bound routers show identity, source, `bound_policy`, candidates, and `policy_decision_executed: false`; their `effective_model` is the base model and their provider chain is empty. No dynamic selection is executed. `--prompt` only affects the existing local static policy-table preview. Nothing is sent upstream for inference; discovery may query provider `/models` endpoints. |
| `bro models [--config PATH] [--provider ID]` | List every routable model selector, each with **all** the providers that can serve it (the fallback chain, in order). Subscription providers are explicit-route-only and therefore appear as pinned `provider:canonical-model` selectors; every displayed selector can be passed unchanged to `bro route`. Tries the running daemon first, falls back to a standalone config parse — same order as `bro route`. The parse is resolved the way the daemon resolves its own config at start-up (built-in defaults, then subscription providers such as `claude-code` re-activated from the OAuth credential store), so a subscription-backed provider is listed with no daemon running; the live table additionally reflects `reload`s and whatever the daemon resolved at start-up. `--json` reports `resolved_via: "live" \| "config"`. Filter with `--provider`. |
| `bro providers list [--config PATH] [--socket PATH]` | Local compatibility output retains `ID  MODELS  ACTIVE  API_BASE`; a named remote context and the dashboard use the redacted accepted catalog without API bases or credentials. Active means configured for routing rather than connectivity-probed. |
| `bro mcp list/show/add/edit/remove [--config PATH]` | Manage local upstream source entries without connecting; see [configuration](#mcp-configuration). |
| `bro mcp check [server] [--config PATH]` | Connect to one configured upstream MCP server, or all of them, and report transport, reachability, latency, negotiated tools capability, and advertised tool names. |
| `bro agents list [--remote] [--config PATH] [--socket PATH]` | Show the accepted ACP catalog + which are configured. A named BitRouter context exposes this catalog only. Local `--remote` also fetches the official ACP agent registry (cdn.agentclientprotocol.com) and lists its agents with version + install support (`npx`/`uvx` stub-able; `manual` for binary-only). |
| `bro agents inspect <agent> [--config PATH]` | Open a fresh ACP session, wait briefly for advertised slash commands, and report which source answers each command. |
| `bro agents check [agent] [--config PATH]` | Preflight one friendly/catalog agent or spawn each configured ACP agent and verify `initialize`. |
| `bro agents conformance <id>` | Run the `acp_compat_1` ACP-compatibility suite against a catalog agent and print the `conformance:` block to record in `registry/runtimes/<runtime>.yaml`. `<id>` is `<runtime>/<harness>` or a bare harness id (`local/` is the default runtime and may be elided). Two tiers: **handshake** (the agent answers `initialize` and settles on the ACP version its registry entry declares) and **routability** (the agent's LLM traffic reaches BitRouter when its routing block is applied). Needs **no provider credentials** — the agent is launched with its own routing pointed at an ephemeral loopback gateway that records what arrived; it does spawn the agent, so the package or binary must be installed. Exits non-zero when a tier fails, and prints no record in that case. |
| `bro agents scaffold <id>` | Print a paste-ready YAML stub for `<id>` — resolved from the bundled catalog first, then the ACP registry (`npx`/`uvx` distributions, version-pinned, `env` included). `agents install` is a hidden compatibility spelling. |
| `bro agents [--config PATH] [--socket PATH] [--limit N] [--after CURSOR]` | Read owner-local live instances: native workers, supervised ACP (including idle), native ACP attachments and managed Core channels. JSON/human, no TTY or implicit startup. `coverage` bounds observation; stopped uses `instances: null`, running observed zero uses `[]`, query failures error. Kind/ID bindings remain distinct; configured agents, historical runs and leases are excluded. Native `model_selector` is requested config; unconfirmed model/resources remain null. Page size is 1–16. Cursors are boot-scoped, sorted pages sample fresh membership. Subcommand target options follow the subcommand. |
| `bro agents sessions [--json] [--config PATH] [--socket PATH]` | List metadata-only supervised run summaries with typed lifecycle, attention, review, native-session identity, route/cost evidence, and attachment state. Its exact list grant cannot retrieve transcript, prompt/result, raw tool/permission context, or exact failure content. A settled turn is **Ready for review**, not “Completed.” |
| `bro agents show <agent-run-id>     # inspect retained state
bro agents events <agent-run-id>   # append retained/live events
| `bro agents stop <agent-run-id> [--config PATH] [--socket PATH]` | Stop and reap the controller/child without deleting the harness-native saved session. Idempotent cleanup preserves a prior failure. |
| `bro agents remove <agent-run-id> [--config PATH] [--socket PATH]` | Remove stopped, failed, or interrupted BitRouter metadata and ephemeral retained events after reaping settles. Does not delete native harness history. |
| `bro observe status [--json] [--config PATH] [--socket PATH]` | OTel exporter snapshot: compiled/wired state, sampler, metric/cardinality counters, and in-flight spans. Local compatibility output retains socket, endpoint, and service fields; named remote contexts and the dashboard use the redacted report, which omits those host details and header values. |

## Durable trajectory operations

Trajectory capture is a local, explicit opt-in and is generic across tasks and
protocols. It never requires task datasets or private routing headers.

```yaml
trajectory:
  enabled: true
  retention_days: 30       # positive; default 30
  outbox_batch_size: 100   # 1..=1000; default 100
```

BitRouter fails closed if a signed lock contains any `progress_guard` while
`trajectory.enabled` is false. Every trajectory setting is restart-only:
reload rejects changes to `enabled`, `retention_days`, or `outbox_batch_size`
and preserves the live last-known-good state. Restart the daemon to apply any
of them. When enabled, capture uses the existing local correlation-key
lifecycle and starts durable outbox publication and startup retention. When
disabled, it creates no correlation key and writes no trajectory ledger rows.

Provider-native Responses continuation remains always active even when
trajectory is disabled. Its independent lifecycle settings are:

```yaml
continuation:
  retention_days: 30       # positive; default 30
  prune_batch_size: 1000   # 1..=10000; default 1000
```

These settings are restart-only. The registry encrypts provider response IDs;
clients see only canonical `brc_` continuation IDs.

| Command | Effect |
|---|---|
| `bro trajectory [--config <PATH>] inspect <EPISODE_ID>` | Resolve the globally unique episode, then report owner-scoped correlation/completeness, structural health, active hold, typed route clauses, and event digests. |
| `bro trajectory [--config <PATH>] replay <EPISODE_ID>` | Audit a stable snapshot and compare the newest persisted route checkpoint digest with replay. Corruption output uses a stable reason code and the first intrinsically invalid event id/sequence only. |
| `bro trajectory [--config <PATH>] prune --before <RFC3339> --dry-run` | Return exact global eligible counts without mutation. |
| `bro trajectory [--config <PATH>] prune --before <RFC3339>` | Transactionally prune delivered old outbox rows and eligible terminal episode history in configured batches. |

`--config <PATH>` is optional and may appear before or after the trajectory leaf
command. Without it, all commands use the standard config resolution chain.
They open the selected source's database: a relative SQLite URL is anchored to
the config file's directory, or to the implicit BitRouter home (normally
`~/.bitrouter`) for zero-config. Resolution neither depends on nor changes the
caller's working directory; absolute/memory SQLite and server URLs are left
unchanged. The global `--json` / `--human` flags work before or after the
subcommand, and no operation takes an owner argument. Inspect/replay resolve the
owner once and keep all subsequent reads owner-filtered. Audit retries a bounded
head→events→head stable read, so a concurrent append yields a valid before/after
snapshot or a contention error, never false corruption. Its prefix reducer
distinguishes a valid route awaiting guard activation from intrinsic route or
guard corruption, so the reported first bad event is exact.

Retention uses an exclusive cutoff. An episode is eligible only when its last
capture is older, every request is settled or failed, and no request points at a
pending outbox row. Explicitly closed episodes must also have an old close time.
Deletion is owner/identity guarded and transactional; no synthetic close event
is written. Startup applies the same rules from `retention_days`. A late native
continuation after pruning becomes a new `unresolved` / `incomplete` episode.

Privacy is write-time, not display-time redaction: durable event JSON, outbox
payloads, Eval evidence, logs, and both report formats contain structural facts,
fixed categories/reason codes, counters, and digests only. They exclude
API/Bearer secrets, prompts, system instructions, tool arguments, file bodies,
and private provider metadata. Trajectory request IDs and Responses native-parent
IDs use the same installation-keyed, owner-bound opaque identity domain. This
does not change external request IDs carried on the wire, sent upstream, or used
for metering joins.

Built-in operational Eval results are always `inconclusive`: metrics describe
counts, streaks, elapsed time, and optional authoritative token/cost facts—not
task quality. Missing metering remains absent instead of becoming zero.
`trajectory.history_complete` is true only for a proven complete prefix;
incomplete/unknown history is not treated as complete and follows the guard's
configured incomplete-history behavior.

Recovery sequence: back up the configured DB → run prune `--dry-run` → inspect
valuable episodes → replay them → run real prune. Retry ordinary audit
contention after traffic quiets. Investigate stable corruption codes or restore
from backup.

## Generic eval exchange

These commands operate on the local append-only evidence ledger. They never
edit or publish `policy-lock.yaml`.

| Command | Effect |
|---|---|
| `bro eval subject seal <DRAFT> --output <SEALED>` | Calculate the canonical digest of redacted evidence, validate the completed subject, and write deterministic JSON. This local file operation never opens the evidence ledger. |
| `bro eval subject put <FILE> [--config PATH]` | Insert an immutable JSON/YAML request, episode, or task subject. |
| `bro eval subject get <EVAL_ID> [--config PATH]` | Read one subject. |
| `bro eval subject list [--config PATH]` | List subjects, including automatically observed routed requests. |
| `bro eval result submit <FILE> [--config PATH]` | Submit a JSON/YAML evaluator result as the local operator; runs the same admission logic as REST. |
| `bro eval snapshot freeze [--at RFC3339] [--config PATH]` | Freeze currently admitted results into a content-addressed manifest. |
| `bro eval snapshot get <SHA256> [--config PATH]` | Read an immutable snapshot manifest. |
| `bro eval status [--config PATH]` | Count subjects and latest admission states. |

Authenticated daemon endpoints mirror the exchange at
`/v1/evals/subjects`, `/v1/evals/results`, `/v1/evals/snapshots`, and
`/v1/evals/status`. External evaluators submit scores; BitRouter owns schema,
identity/metric authority, conflict/holdout admission, and snapshots.
The CLI uses the local ownership scope; authenticated REST is isolated by the
virtual key's owning user. Snapshot roots commit both subject and result content.

## ACP sessions

Explicit external ACP execution modes share one controller and differ only in who drives it. `acp serve <agent>` exposes the connection-level controller over **stdio** for an ACP client. `run` drives one prompt **in-process** and presents it as NDJSON/text/quiet output. Session ownership is harness-native in both. Hidden `spawn` and `acp prompt` spellings remain for migration. Both **attempt to route the harness's model traffic through the daemon when the headless adapter supports redirection** — add `--direct` / `--base-url` / `--model` / `--no-start`.

| Command | Effect |
|---|---|
| `bro run <agent> [prompt\|-] [--prompt-file PATH] [--load ID\|--resume ID] [--cwd PATH] [--turn-timeout SECS] [--approve-all\|--approve-reads\|--deny-all] [--permission-policy JSON\|@PATH] [--result-schema JSON\|@PATH] [--background [--allow-shared-directory]] [--format ndjson\|text\|quiet] [routing flags]` | Foreground is the canonical one-shot headless entry and streams versioned NDJSON. `--background` instead returns only after the resident local supervisor owns an attachable run ID; it conflicts with hidden `--no-wait`. Load/resume remain capability-gated explicit native-session selections. |
| `bro acp serve [--model MODEL] [--read-only] [--turn-timeout SECS] [--no-start] [--config PATH]` | Native BRO ACP v1/draft v2 over stdio, backed by the daemon-owned ThreadService. Model falls back to `chat.model`. EOF detaches; cancel pauses retained inputs; close cancels active and queued work and preserves history. Load/resume never resumes the queue. See `references/sessions.md` for approval recovery and `_bitrouter/session/resume_queue`. |
| `bro acp serve <agent> [--turn-timeout SECS] [routing flags] [--config PATH]` | Expose an ACP-compatible adapter over protocol-pure **stdio** until this ACP client disconnects. The client initializes first, may open multiple harness-native sessions, and owns prompt deadlines. Session IDs, history, and storage remain harness-owned. |

**External controller lifecycle**: ACP client `initialize` capabilities and `_meta` reach the harness exactly. Each `session/new` is forwarded and returns that harness response's opaque `sessionId`; repeated calls may create different sessions. Advertised `session/list|load|resume|fork|close|delete`, prompts, cancellations, callbacks, updates, errors, `_meta`, and extension payloads pass through. BitRouter neither mints a client-facing session alias nor keeps a session catalog.

**Endpoint setup**: Claude uses pinned `@agentclientprotocol/claude-agent-acp@0.75.1`; Codex uses pinned `@agentclientprotocol/codex-acp@1.10.0`. Controller-to-harness `providers/*` configures the model endpoint and is removed from client-facing capabilities. Claude's fallback is `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` / newline-separated `ANTHROPIC_CUSTOM_HEADERS`; Codex's is `CODEX_CONFIG` plus `MODEL_PROVIDER`, with no ACP-mode `-c` arguments.

**Session route control**: a locally bound controller advertises stable-v1
`_bitrouter/route/list|set|reset` methods under initialize response
`_meta["bitrouter.dev/controller"].routeControl`. They operate on opaque native
`sessionId` values and create daemon-confirmed ephemeral leases; client-side
`providers/*` remains unavailable. `--direct` and explicit remote `--base-url`
connections do not advertise route control because hosted HTTP route control
is not implemented yet. `route/list.available` is a logical-model suggestion list;
presets and permitted explicit routes remain validated free-form `set` inputs.

**Session cost**: the same binding advertises
`_meta["bitrouter.dev/controller"].usage` (`version: "1"`, `scope: "session"`,
`fields: ["cost"]`, `provenance: "bitrouter.dev/cost"`). The controller then
decorates the harness's own `usage_update` notifications: `used` and `size`
pass through untouched and `cost` becomes the spend BitRouter metered for that
native session and its child agents, marked
`update._meta["bitrouter.dev/cost"] = "router"`. No usage update is ever
synthesized, and unmetered traffic (`--direct`, explicit `--base-url`, own-auth
harnesses, sessions with no priced requests) is forwarded exactly as sent —
never `$0.00`, never a daemon-wide figure.

**External observability and turns**: `acp serve <agent>` forwards the harness's session/cancel and session/update wire unchanged, except that a locally bound controller decorates the harness's own `usage_update` with session-attributed `cost` (see **Session cost**); it never synthesizes per-session usage or timeout behavior. Authenticated routed model calls normalize BitRouter's static controller/harness headers and the harness's native Claude/Codex identity into controlled capture/replay, request spans, route decisions, and nullable metering correlation. The normal API/virtual key is the only authentication boundary; authorization, cookies, and credentials remain excluded. `run` drives the same controller in-process, so it gets identical forwarding; its OTel turn spans are re-derived from the prompt round-trip and correlate on the **harness-native** session id. `--turn-timeout` and cooperative cancellation are the client's there, and in `code <agent>`: every command now drives the same controller through the same client, and no local `record_id` or controller-owned FIFO queue exists. Code may keep an explicit process-local next-turn queue.

**NDJSON format**: every event carries `"version":1`, a monotonically increasing `"seq"`, and a `"type"`. The first success event is `session`; streamed events include `message_chunk`, `thought_chunk`, `tool_call`, `tool_call_update`, `usage`, and `permission`; exactly one terminal `result` or `error` follows. The old `--format json` value remains an alias for `ndjson` during migration.

**Headless permissions** (`run`, plus the hidden compatibility forms): nobody is at the terminal, so the caller states the rule. `--deny-all` (the default) answers every `session/request_permission` with the agent's reject option; `--approve-reads` approves calls the harness labels `read` or `search` (the ACP tool `kind`) and denies the rest, unlabelled calls included; `--approve-all` approves everything. `--permission-policy '{"autoApprove":["read","Grep"],"autoDeny":["execute"],"defaultAction":"deny"}'` (or `@path`) overrides per tool: entries match the tool kind, the tool-call title, or the title's first word, case-insensitively; `autoDeny` beats `autoApprove`, and an unmatched request falls to `defaultAction`, else the mode flag. An approval against a request that offered no allow option still resolves to the reject option and counts as a denial. Each answer is one NDJSON line, `{"type":"permission","decision":"approved"|"denied","title":"…","kind":"edit"|null}`. **Exit status 5** when at least one request was denied and none approved; 0 otherwise (a turn or launch failure is still 1). Use `run` for one headless turn; its permission policy remains explicit.

**Background permissions and safety**: `run --background` has an interactive supervisor broker, so no permission flag means unmatched requests use **Ask** and the run becomes Needs input. Explicit `--approve-all`, `--approve-reads`, or `--deny-all` retain the foreground meanings. A policy's `defaultAction` wins; without one, exact list matches apply and an unmatched request uses the explicit mode or background Ask. `--turn-timeout` includes permission wait time and records a timeout failure after cancellation. A result schema is validated before launch and before Ready. The supervisor claims the canonical Git worktree root (or canonical non-Git directory); collisions fail before process launch. Prefer a separate worktree. `--allow-shared-directory` is a background-only warned override of the claim, never a permission expansion.

**Result contract** (`run --result-schema '<JSON Schema>'`, or `@path` to read it from a file; conflicts with `--no-wait`): the schema rides the subagent's prompt as an instruction to end the reply with a ```json fenced block. The reply's **last** ```json block (or a bare-JSON reply) is extracted and validated; on a missing/invalid result the subagent gets **one** repair re-prompt. The terminal line then carries the machine-consumable outcome — success: `{"type":"result","stop_reason":…,"result":{…},"schema_ok":true}`; failure after repair: `…,"result":null,"schema_ok":false,"raw":"<last reply text>"` (the orchestrator is never blocked). Bare `run` output is unchanged (no `result`/`schema_ok`/`raw` keys). A malformed schema fails fast before any session side effect.

See `references/sessions.md` for the controller/native-session boundary and what `run` adds on top of it.

## Removed first-party UI

`code`, `tui`, `chat` and `agents attach` return migration guidance before
local or remote I/O and are absent from normal help. Use the single-operation
controls above, `bro run`, or `bro launch <agent>`. There is no replacement REPL.

`chat.model` and `chat.commands` remain headless defaults. `chat.agent` only
selects an explicit onboarding native launch. `bro config validate` reports
the migration when a saved agent exists. `code-hotkeys.json` and UI bindings are
no longer read; configuration and durable history are left intact.

## Setup helpers

For legacy YAML, `bro config migrate-routers -c bitrouter.yaml --candidate
routers.candidate.yaml` writes a reviewable candidate and reports `source_digest`.
Apply with the same paths plus `--apply --source-digest <reported-digest>`;
stale source/candidate changes are refused, a backup is retained, and activation
requires a restart. Unsupported YAML needs manual migration. `@name[:variant]`
remains a compatibility address after conversion; use `bitrouter/name` for new
calls. Router access policies authorize the requested alias, delegating model
selection; a physical-model-only allowlist does not authorize the alias.

| Command | Effect |
|---|---|
| `bro init [--yes] [--force] [--reset] [-c PATH] [credential/resource flags] [--harness claude\|codex] [--after launch\|serve\|exit] [--model ID]` | Save Router setup and optional ACP default/model in the resolved configuration, or BitRouter home when absent. Credential flags: `--cloud-login`, `--api-key`, `--provider`, `--provider-api-key`, `--use-detected`. Headless setup reports-and-skips interactive logins. `--after launch` explicitly launches the selected native tool; `--force` resets existing configuration. |
| `bro config validate [--config PATH]` | Validate a config file by running the real parse path: structure (deserialization), `derives` resolution, the upstream-URL (SSRF) gate, and any referenced `policy-lock.yaml`. Exits non-zero on an invalid config — **CI-safe**. Does *not* load the JSON Schema (that artifact, at `dist/schema/bitrouter.config.schema.json` / regenerated with `cargo run -p dist-helper -- generate-schema`, is for IDE autocomplete + the drift check). Unset `${VAR}` references are substituted with a `.invalid` placeholder and reported as warnings, so secrets need not be present; a value that embeds one mid-string is not authoritatively checked. Also reports `ignored_config` — `plugins.<id>` blocks the binary does not read and therefore ignores, which is otherwise silent (`bitrouter-policy` and `bitrouter-telemetry` are the supported ids). Unknown ids do **not** fail validation. The removed `plugins.bitrouter-guardrails` key is different: any presence fails validation and activation; see `references/guardrails.md` for migration and input-only coverage. The daemon, `bro acp serve` and `bro run` log the same set on every start. |
| `bro skills list [--workspace PATH\|--global] [--json\|--human]` | Inspect BitRouter-managed installations plus unmanaged discoveries, including invalid and retained versions; see [versioned skills](#versioned-skills). |
| `bro skills show NAME_OR_ID [--workspace PATH\|--global]` | Exact scope, installation identity, current version/provenance and retained history; no activation. |
| `bro skills install SOURCE [--skill NAME_OR_RELATIVE_PATH] [--ref REF] [--workspace PATH\|--global]` | Snapshot a complete local/Git bundle; validate and atomically publish its version, receipt and alias without executing hooks/scripts. |
| `bro skills update NAME_OR_ID [--ref REF] [--workspace PATH\|--global]` | Resnapshot a local source or tracked Git branch. Tag/commit pins stay fixed unless a different explicit ref is provided. Failure preserves the previous alias. |
| `bro skills uninstall NAME_OR_ID [--workspace PATH\|--global]` | Remove the managed alias, retain every version and receipt; never edit unmanaged directories. |
| `bro skills explain --thread THREAD_ID --turn TURN_ID [--config PATH]` | Read original native skill selection, exact versions/entry, consent, verified-byte commitments and prepared SDK request evidence without a daemon or resource connections. Managed inputs use `--managed-session NAME --request-id KEY`; `--agent-turn ID` narrows the work unit. |
| `bro skills init <NAME> [--output PATH] [--json\|--human]` | Scaffold a spec-valid skill directory — writes `<NAME>/SKILL.md` unless `--output` names a path. `<NAME>` is written into the generated frontmatter. |
| `bro policy create <id> [--dir DIR]` | Write a starter access-control policy file under `--dir` (default `./policies`). Bind to a key with `bro key sign --user <id> --policy <id>`. |
| `bro policy init <name> [--router <id>] --economy <model> [--economy-effort <level>] [--strong <model>] [--strong-effort <level>] [--config PATH]` | Create or reuse deterministic `policy-lock.yaml` and bind the policy to `bitrouter/<id>`. With neither binding flag, the router id defaults to `coding`. The caller chooses both models; an equivalent existing router can supply its strong base model when `--strong` is omitted. Router initialization preserves `policy.mode`, `chat`, and harness settings; the default mode remains `frozen`. Explicit supported efforts make `(model, effort)` the target identity. `--preset <preset>` is the mutually exclusive legacy form: it binds `@preset[:variant]`, may infer that preset's strong model, and retains its historical `policy.mode: adaptive` behavior. |
| `bro policy check [--config PATH]` | Cross-validate the local main config and lock. |
| `bro policy status [--view active\|disk] [--config PATH] [--socket PATH]` | Report an explicit policy source, digest, mode, policies, and preset bindings. Local default is `disk`; named remote context default is `active`. |
| `bro policy show <name> [--view active\|disk] [--config PATH] [--socket PATH]` | Print one named policy from the explicit source; the same local/remote defaults apply. |
| `bro policy compile --output FILE [--eval-snapshot SHA256] [--snapshot-time UNIX_MS] [--config PATH]` | Compile a frozen generic-eval snapshot into a deterministic v4 candidate. Never changes the active lock. |
| `bro policy diff <ACTIVE> <CANDIDATE>` | Compare explicit route selections. |
| `bro policy publish <CANDIDATE> [--config PATH] [--socket PATH]` | Publish that exact compiled v4 candidate under adaptive mode using its parent digest as a compare-and-swap token. |
| `bro policy verify --evidence [--config PATH]` | Reconstruct the active compiled lock's evidence root from the local ledger/snapshot. |
| `bro policy evolve [--apply \| --output FILE] [--config PATH]` | Compatibility compile/publish command. `--apply` requires `policy.mode: adaptive`; request-time routing remains lock-only. |
| `bro policy reload [--config PATH] [--socket PATH]` | Hot-reload main config and policy lock through the existing daemon control socket. Invalid locks preserve the last-known-good runtime snapshot. |
| `bro policy rollback <DIGEST> [--config PATH] [--socket PATH]` | Restore exact lock bytes from local promotion history, then reload or restore on rejection. |
| `bro workflow-state classifier-bakeoff --fixtures DIR [--submission FILE] --output FILE` | Build a deterministic, read-only Route Context V3 research report (artifact/report schema v2: corrected ECE, null unobserved selective risk, and `classification_surrogate_loss`). Omit `--submission` for the compiled scorecard baseline. Candidate files are strict full-coverage shadow evidence and never alter live routing. |
| `bro optimize run [--policy auto] [--candidate-tier TIER] [--exploration-ppm N] [--minimum-tasks N] [--maximum-tasks N] [--minimum-pass-rate-ppm N] [--evaluator-config-digest SHA256] [--config PATH] [--socket PATH]` | Perform and autonomously publish one deterministic history-driven controller step. Omit `--candidate-tier` to use the signed policy's `adequacy.explore_tier`; pass it only to override that tier. Champion-only history can start signed exploration; later steps promote, retreat, hold, or converge. The invocation itself grants publication authority; there is no separate review or approval command. |
| `bro optimize status [--policy auto] [--config PATH]` | Observe the signed policy state without changing files or the database: `exploring` means an experiment is active and `idle` means none is active. Status does not infer convergence; repeat normal traced agent/Terminal Bench work, external Eval submission, and `run` until `optimize run` reports `converged`. Only complete task/episode cohorts gate quality and complete-unit cost; request subjects rank opportunities only. |
| `bro key sign --user <id> [--db URL] [--policy ID]` | Mint a `brvk_…` virtual key in the auth DB. Plaintext is shown once; only its SHA-256 hash is stored. Default DB is `sqlite://./bitrouter.db`. |

Adaptive routing uses a source-independent predictor selected by
`key_strategy: agent_trace`. Its static policy keys are exclusively canonical
`semantic_route/v1|<task-family>|<role>|<risk>` values. Native runtime adapters add
diagnostics only, not policy keys, and private BitRouter headers are not needed.
Observed `agent_trace/v2` values remain telemetry; retired route shapes and
`key_strategy: legacy_fingerprint` are rejected during configuration
validation. `adequacy.explore_opening: true` enables exploration for
source-neutral opening projections. The removed
`adequacy.max_downgraded_requests_per_session` setting is rejected because
session identity is diagnostic-only.

`policy.mode` belongs to the running process and defaults to `frozen`. Frozen
mode ignores evidence-ledger rows for live routing and forbids active lock
replacement while continuing to record evidence. `adaptive` permits validated
writeback but routes identically from the lock. Legacy
`writeback: locked|evolve` input parses as `frozen|adaptive`, but new config and
status output use only `mode`.

## Per-provider OAuth

| Command | Effect |
|---|---|
| `bro providers login <provider>` | Per-provider OAuth. Supported providers include **`claude-code`**, **`github-copilot`**, and **`openai-codex`** — runs or adopts the provider's login flow and stores the refreshing token under `$XDG_DATA_HOME/bitrouter/oauth-tokens.json`. |
| `bro providers login <provider> --api-key <KEY>` / `--key-stdin` | Seed a BYOK provider (any that accepts a pasted key — `openai`, `anthropic`, `google`, `openrouter`, `opencode-*`) non-interactively: skips the method menu and the stdin paste. `--key-stdin` reads one line from stdin instead. Both conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method. For `bitrouter`, the key seeds the cloud credential (same as `cloud login --api-key`). |
| `bro providers logout <provider>` | Remove the stored OAuth token or credential for `<provider>`. |

## BitRouter Cloud sign-in (`bro cloud …`)

OAuth 2.0 device-flow or non-interactive API-key sign-in against BitRouter Cloud. The persisted credential drives the raw API client, the `bitrouter` provider in the local daemon, telemetry attribution, and the management subcommands below.

| Command | Effect |
|---|---|
| `bro cloud login [--oauth-as URL] [--client-id ID] [--scope SCOPE]` | RFC 8628 device-flow login. Prints an approval URL, polls the token endpoint, and persists access + refresh tokens to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (mode 0600 on Unix). Auto-refreshes within 60 s of access-token expiry on every subsequent call. Defaults: AS `https://api.bitrouter.ai`, client id `bitrouter-cli`, scope set covering `inference:invoke usage:read keys:* billing:read policy:* byok:* namespace:read`. |
| `bro cloud login --api-key <BRK_API_KEY> [--oauth-as URL]` | Non-interactive CI login. Validates `brk_<token_id>.<secret>` and stores it without a network request. Conflicts with OAuth-only `--client-id` and `--scope`; never prints the key. |
| `bro cloud logout` | OAuth: best-effort RFC 7009 revoke, then delete the local file. API key: local deletion only. |
| `bro cloud whoami` | Print auth type (`oauth` or `api_key`) and non-secret local metadata. Reads the on-disk file only — no network. |

## BitRouter Cloud raw API (`bro cloud api`)

`bro cloud api <relative-endpoint>` mirrors the core `gh api` workflow and reuses either stored credential. It accepts arbitrary relative paths but never follows redirects or sends credentials off the login origin.

| Flag | Effect |
|---|---|
| `-X, --method METHOD` | Explicit method; implicit `GET`, or `POST` when fields/input are supplied. |
| `-H, --header KEY:VALUE` | Repeatable request header. User `Authorization` overrides the stored bearer. |
| `-f, --raw-field KEY=VALUE` | String JSON/query field with nested `key[sub]` / `key[]` grammar; bare `key[]` creates an empty array. |
| `-F, --field KEY=VALUE` | Typed bool/null/integer field, or `@file` / `@-` string content. |
| `--input FILE|-` | Exact request body; fields move to the query string. |
| `-i, --include` | Status line + response headers before body. |
| `--silent` | Drain without printing the body. |
| `--verbose` | Redacted method/URL/header/status diagnostics on stderr. |

Non-TTY JSON, binary bodies, and SSE stream byte-for-byte to stdout. HTTP 4xx/5xx preserves the body on stdout, writes the error to stderr, and exits non-zero. Tested endpoints include models, providers, public usage stats, Chat Completions, Messages, Responses, settlement receipts, routing presets, OAuth clients, billing ledger/status routes, and namespace/account management routes.

Side effect: when the credentials file exists, the local daemon auto-adds the `bitrouter` provider to the zero-config providers map, so every model your account is entitled to is routable as `bitrouter:<model-id>` against `localhost:4356` without further configuration.

## BitRouter Cloud management (`bro cloud …`)

Typed wrappers over the common Cloud management workflows. Requires either login form first. OAuth credentials use their baked namespace; API keys use `/v1/namespaces/me/*`. Every typed leaf accepts `--json` for raw response output; default is a `systemctl`-style key:value block (single resource) or a small table (lists). Use `bro cloud api <relative-endpoint>` for Cloud APIs that do not yet have typed CLI wrappers. On a 403 with `missing required scope: <s>`, OAuth users receive a copy-pasteable `--scope` re-login hint; API-key users are directed to a key with that scope.

| Command | Effect |
|---|---|
| `bro cloud whoami` | Cloud base URL + local subject/scope from the credentials file. Offline. |
| `bro cloud namespace list/current` | Inspect available workspaces and the one this CLI session is baked to. Lifecycle operations require the Console or raw `cloud api` with the right control-plane scope. |
| `bro cloud keys list / mint / revoke` | List `brk_…` API keys, mint a new one (plaintext shown once), revoke by id. Scopes: `keys:read` / `keys:write`. |
| `bro cloud usage [--from RFC3339] [--to RFC3339]` | Aggregate spend (micro-USD) + token counts over a window (default last 30 days). Scope: `usage:read`. |
| `bro cloud requests [--limit N] [--offset N]` | Paged request history. Scope: `usage:read`. |
| `bro cloud billing balance` | Credit balance + pending debits + available (`max(balance - pending, 0)`). Scope: `billing:read`. |
| `bro cloud billing checkout --amount-cents N` | Start a Stripe checkout session for a credit top-up. Returns a hosted URL. Scope: `billing:write` (opt-in via `--scope` at login). |
| `bro cloud policy list/get/create/update/delete/bind/unbind/disable/enable/bindings/effective/for-principal` | Generic CRUD over policy registry. `create` and `update --spec` accept a JSON file path or `-` for stdin. `effective` and `for-principal` answer "what would happen for this principal" without making an actual inference call. Scope: `policy:read` / `policy:write`. |
| `bro cloud budget list/get/create/update/delete` | Typed sugar over budget-kind policies. |
| `bro cloud preset list/get/create/update/delete` | Typed sugar over preset-kind policies. |
| `bro cloud byok list/set/delete` | BYOK provider keys. `set` takes already-sealed ciphertext (`--ciphertext-b64` + `--kek-id` matching the cloud's current X25519 public key). Scope: `byok:read` / `byok:write`. |

## Native launch and headless ACP

The maintained ACP adapters are pinned to
`@agentclientprotocol/codex-acp@1.10.0` and
`@agentclientprotocol/claude-agent-acp@0.75.1`. Node.js 22+ and `npx` are
required. At ACP session startup, BitRouter probes for compatible installed
Codex and Claude CLIs behind the adapters; explicit env and transport
overrides win, and custom adapter versions are not modified.

The native shortcuts and `launch` run a harness's **interactive native TUI**;
`run` drives an **ACP-compatible adapter as a headless sub-agent**. Both route the
harness's LLM traffic through the daemon, drawing per-harness routing knowledge
from one shared catalog, so native `launch claude` and ACP `run claude` resolve
the appropriate facets of the same harness entry.

| Command | Effect |
|---|---|
| `bro launch <agent> [options] -- <agent args...>` | Launch a coding-agent CLI's native interface through BitRouter without editing its user config. `bro claude`, `bro claude-code`, and `bro codex` are first-class shortcuts. The hidden `launch --agent` spelling remains compatible. When supported, the launcher injects the daemon's `bitrouter_tools` aggregate gateway so configured upstream MCP servers reach the harness. |
The former `spawn` command is hidden for compatibility: `spawn -p` maps to `run`, `spawn --serve` to `acp serve`, and `spawn --check` to `agents check`.

**Routing (attempted by default)** for `run` and `acp serve <agent>`:
- `--direct` — do **not** route through the daemon; the harness uses its own provider auth.
- `--model <id>` — pin the harness's model (its model env var, or `-c model=` for codex).
- `--base-url <URL>` — override the gateway URL (else derived from `server.listen`).
- `--no-start` — never auto-start a local daemon; fail fast if it's down.
- Session flags (`--turn-timeout`) match `acp`.
- Auth: routed sub-agents authenticate with `BITROUTER_API_KEY` when set, else a local placeholder (fine under `skip_auth: true`); under `skip_auth: false` a key is required or `run` fails fast with `auth_required`.
- Fail-fast: if the daemon is unreachable (after auto-start) or auth is required and absent, `run` emits a single structured error before any session side effect and exits non-zero; `acp serve` reports the diagnostic on stderr. Catalog adapters whose routing is config-synthesis only and non-catalog agents warn and run direct.
- `bro spawn --agent <claude\|codex> …` is a **deprecated alias** for `bro launch` (prints a migration note).

**`run` first line** is a versioned `session` correlation line carrying the native session id, resolved agent, route endpoint, and launch id; `launch_id` joins it to daemon cost/metering. Then the normal sequenced NDJSON update stream follows.


## Not in the proxy binary

- `bro wallet` does **not** exist — it exits with `unrecognized subcommand`. OWS wallet integration lives in the separate `ows` workspace and is unlikely to land here.

## Config resolution

Every command that takes `--config` resolves the path in this order when the flag is omitted:

1. `./bitrouter.yaml` (current working directory)
2. `$BITROUTER_HOME/bitrouter.yaml`
3. `~/.bitrouter/bitrouter.yaml`
4. Zero-config in-memory defaults (no file)

The daemon `chdir`s to the directory holding the resolved config on startup, so every relative path inside the config (`database.url: sqlite://./bitrouter.db`, policy/agent file references) resolves against that directory, not the launcher's CWD.

## Signals

| Signal | Behavior |
|---|---|
| SIGHUP | Hot-reload daemon-side config + routing table. It does not forward provider keys from the invoking shell; use `bro reload` for env-var rotation. |
| SIGINT / SIGTERM | Graceful shutdown: flush OTel exporter, remove pid file, exit 0. |
| SIGKILL | No cleanup — pid file will be stale and `bro status` will report it. `bro start` cleans up stale pid files automatically before launching. |

## Router request-check diagnostics

There is no `bro checks` command, checker probe or request-check receipt API.
Use `bro config validate` for static declarations. A custom host verifies linked
registrations and revisions at startup, and emits bounded content-free tracing
for native invocations. Checker declarations and router bindings require restart.
Settled token/cost history remains under `bro requests`, but does not prove that
a particular native check ran.

## Native project instructions, MCP and skills wiring

Native Threads load startup instructions as durable **user context**, including
read-only Threads. The daemon's BitRouter home (normally `~/.bitrouter`) supplies
global `AGENTS.override.md`, or the first nonempty `AGENTS.md`. Project discovery
loads the Git root through the selected workspace/working directory, choosing
one file per directory: `AGENTS.override.md`, `AGENTS.md`, then host-configured
fallback names. No Git root means only the working directory is checked.
Local authenticated workspace registration authorizes ancestor instruction reads
up to the nearest Git root without expanding native tool permissions. Remote
hosts must grant the ancestor discovery range; otherwise it stops at the grant.
The project chain has a shared 32 KiB default budget (host configurable up to
64 KiB); global content is bounded separately at 64 KiB. Excess content is
truncated with a persisted warning; invalid UTF-8 uses replacement characters.
Directories are skipped; symlinks must resolve within their instruction root.
The complete prompt counts toward the model-context bound and grants no tools.

System instructions define directory scope and deeper-rule precedence and ask
the model to check deeper directories with `read`. Descendants are not scanned
or injected automatically. A live Thread retains its startup snapshot across
Turns, including an empty snapshot. A new Thread discovers again. After server
restart, authorized continuation refreshes instructions only at a settled model
boundary and records replacements/removals explicitly; known effects are never
replayed. Completing an already settled outcome and cold browsing reread no
instruction files. Runtime format 5 persists snapshots and private ACP bindings/close results; formats 2/3/4 remain readable.

The daemon supplies its configured `mcp_servers` to native coding Turns. MCP tools
use the existing approval, exclusive execution and durable result barriers;
unknown outcomes are not retried. Read-only Turns expose no MCP tools and connect
only to skill sources explicitly granted as described below.
`bro mcp check` uses this client and honors `mcp.upstream_protocol`. Native
transport/credential bindings are fixed at startup; restart to change them.
Cold Thread browsing, history and loading do not spawn MCP servers.

Active Turns discover workspace skills plus the daemon user's `.agents/skills`,
`.codex/skills`, `.claude/skills` and `$CODEX_HOME/skills` when configured. Runtime
snapshots expose metadata and hashed versions. Discovery does not inject skill
bodies or MCP instructions into the prompt, install skills or execute scripts.

Native skill sources default to none. To admit MCP metadata, the operator can
configure a source using its `mcp_servers` map key:

```yaml
skills:
  mcp_sources:
    reviews:
      workspaces: [/absolute/project]
      local: true
      api_key_ids: []
      user_ids: []
```

This grants the synthetic local caller on that exact workspace. Authenticated
callers instead need a matching key or user ID; the source policy does not grant
workspace/tool permissions. Relative workspaces fail configuration validation.
An absent configured server stays unavailable, so removing MCP configuration
does not preserve usable source access or require deleting its policy first.
Restart to apply startup-owned source/transport policy changes.

G09 currently admits full entries into native resource inventory and provides
source-bound, manifest-bound on-demand reads that verify raw size/hash and the
complete root frontmatter. Listings fetch no files. Optional dynamic entries are
excluded explicitly; direct dynamic lookup fails. Every supplied cache hit is
reverified. Native Core can select entries, obtain exact per-skill consent and
load verified root instructions as source-tagged user data; see the task controls
above. Original root bytes persist in WorkUnit and are reverified after resource
source disconnect or authenticated Core restoration. Native `skill_read` delivers verified bound supporting files as Turn-scoped user
data, checked against frozen Turn and final model/context budgets. Original
root/file caches survive authorized journal restoration; lost owners remain
recovery-blocked. Supervised Codex/Claude local prompt delivery has separate capabilities; MCP-origin external delivery is unsupported and real service qualification remains unverified.
`allowed-tools` does not grant tools. Local `skills list/show`
and cold history do not contact these sources.

Skill URI syntax preserves the complete original identity, including rootless
schemes, spelling, query and fragment. Paths determine the skill directory and
parent name. Relative file references remain inside the held manifest; multiple
matching URIs require an exact URI rather than a default choice. The URI's
authority is never resolved as a host. CLI selectors accept these same URI forms.

### Versioned skills

For daemon-supervised external ACP, `run codex|claude PROMPT --background`
accepts repeatable `--skill project:ID`, `--skill global:ID` and
`--exclude-skill SELECTOR`. Each input uses the original library and decision
backend; optional automatic selection is empty without that backend. Required
entries take priority and must fit four frames / 64 KiB, with a 2 MiB serialized
ACP request ceiling. Frames bind original receipt, whole-bundle version, path
and exact input. Codex receives text; Claude requires negotiated embeddedContext
and receives text resources as user context. `agents list` reports path contracts,
not live readiness; `agents show RUN` includes negotiated skill capability and
original per-input decision/usage/delivery receipts. Those facts share the
original supervisor ledger, capped at 64 reports / 4 MiB per run. Restart does
not replay an uncertain result or make a lost owner runnable.

MCP-origin required entries are unsupported, even with `--approve-all`. Unknown
adapters, foreground/transparent ACP, native launch and inference-only routing
retain existing functions but do not support this allocation. Slash commands
retain their original shape and skip automatic skill selection; required skills
on the same input are unsupported. Prompt instructions do not establish native
activation, protected support-file access, tool grants or vendor token/history
admission. Omitting a skill later does not remove earlier vendor history. Real
Codex/Claude sessions remain unverified; consult their harness references.

Project scope is explicit `--workspace PATH`, otherwise the nearest Git root or
current directory. The library is `<project>/.bitrouter/skills`; `--global`
selects `<BITROUTER_HOME-or-default-home>/skills`. Flags conflict. `installed.json`
owns UUID installation identities, aliases and provenance/version histories;
`versions/<digest>` stores complete immutable bundles. Reports include principal,
scope/root/library, catalog revision, managed/unmanaged ownership, exact digest,
source, state and `activation: not_requested`.

Native discovery uses an existing explicit workspace index before the nearest
Git-root library. Empty/invalid/unsafe explicit scopes do not fall back outward.
Creating a child library cannot redirect an existing Turn's original version;
reads remain bound to its caller/workspace and authorized library root.

Names use 1–64 Unicode lowercase alphanumeric characters and single hyphens;
count characters rather than UTF-8 bytes. Uppercase, leading/trailing or repeated
hyphens, path separators and emoji are rejected. The same validator serves local
installations and MCP entries; encoded URI parent names are compared after decoding.

Use a local directory or Git URL; `git+/absolute/repository` selects local Git
objects. `--skill NAME` or `--skill ./relative/path` resolves multi-skill sources;
`.` selects a root bundle. Nonterminal ambiguity fails. `--ref` is Git-only.
Receipts retain selected path and local source or requested/resolved ref, exact
commit and branch-tracking policy. Pinned updates work offline without fetching
unless the user chooses a different ref. Git credentials belong in existing Git
authentication config, not URLs. Git terminal prompting is disabled; SSH uses
batch mode. Live remote authentication remains unverified.

Limits are 32 MiB, 1,024 regular files, 256 KiB `SKILL.md` and 4,096 directories.
The digest includes all paths/content, empty directories and executable flags.
Links, escapes and special entries are refused. Git acquires fixed objects
without checkout, submodules, source hooks or filters. Scripts are inert data.
Scope lock/CAS and atomic publication prevent stale writers or failed updates
from replacing an alias. Installed-content tampering is reported, not overwritten.
After a post-replacement sync error inspect `list/show` before retrying.

Uninstall reports `uninstalled_retained`; a later CLI process can read the old
versions/receipt. Reinstall gets a new installation ID. There is no GC. Project
conventional skill layouts and user `.agents`, `.codex`, `.claude`, `$CODEX_HOME`
roots remain unmanaged inspections. Commands never move/delete/overwrite those
contents, start the daemon/MCP/agent, or change global harness configuration.
`init` still only scaffolds source and does not install it.

Native hosts now read active project/user aliases from that same installation
store, retaining independent scope, installation ID, whole-bundle version,
original receipt/provenance, caller/workspace and catalog revision. Listing
does not prefetch bodies. Version readers verify manifest containment, real
directories, raw file size/hash and executable mode on use. Updates/uninstalls
leave original receipts available to a bound-version reader; an absent old
version fails explicitly. An unavailable scope is reported separately.

Catalog metadata can refresh between native Turns without changing tool grants.
Native Core now assesses managed candidates through the configured decision
backend, freezes complete receipts and bounded choices in the original context
store's WorkUnit, and commits verified instructions before model preparation.
Without a decision backend, optional automatic choices remain empty with an
explicit reason. Library callers can require/exclude scope/installation IDs in
`TaskInput.routing.skills`; these constraints take priority and survive child
assignment, while automatic parent choices are not copied into the child.
Every later model step rechecks original bytes; alias changes never substitute
a new version. MCP root caches remain in the original WorkUnit and revalidate
source authority and original bytes; they never become filesystem candidates.
Explicit MCP inclusions use direct metadata lookup even when not listed and
still require separate consent. Installation alone does not activate a skill.
Native `skill_read` delivers verified bound UTF-8 supporting files as Turn-scoped
user data; original native budgets and permission checks remain enforced. No local origin MCP is added.

`skills explain --thread THREAD_ID --turn TURN_ID` reads the original native
Thread log. Managed history uses `--managed-session NAME --request-id KEY`;
`--agent-turn ID` selects one exact work unit. `--config PATH` selects the database
address without loading providers/MCP; alternatively use `--database-url URL`.
SQLite is forced to read-only mode and missing databases are not created. Cold
inspection neither claims an owner nor resumes work; it works with missing current
source/workspace files or an unreleased lost owner. It reports selected, verified
bytes and exact prepared SDK contexts separately. Available attempt/HTTP outcomes
remain separate; `attempts: null` preserves unavailable historical execution detail.
Supporting files report their original reference, raw digest/size and which
prepared SDK inputs contain the exact user data frame; bodies are not printed.
Prepared context is not proof of a network send or model obedience. JSON and
`--human` are supported; named remote-control contexts cannot fall back locally.

`bro workflow-state reliability-report --database-url <URL> --config <PATH> --policy <NAME> --output <PATH>` reads reliability thresholds from the named policy in the version 4 lock (`--policy` defaults to `auto`).

### Native model history across protocol boundaries

Native tool errors and execution denials remain typed in the durable journal.
Model prompts represent them as JSON containing an explicit `status` and the
original typed `output`, so Chat Completions and Responses preserve the failure
meaning without a protocol-native error flag. Prompt byte limits include this
representation.

## MCP configuration

`mcp list` reads configured servers; `show NAME` shows one. Reports name the absolute source, include each map key as id and its declared name, and redact headers/env, arguments and URL userinfo/query/fragment. Old aliases remain readable; writes require matching key/name.
`add NAME --url URL` and `add NAME --command PROGRAM -- ARGS...` save an entry.
`add/edit NAME --from FILE|-` accepts one complete YAML/JSON McpServerConfig: name, transport (http URL/headers or stdio command/args/env), aggregate and tool_prefix. Name must match the map key; --from conflicts with shortcuts. Edit replaces one whole entry; omitted fields use structural defaults. Add conflicts if the name exists; edit/remove require it.

Use `mcp edit NAME` with `$EDITOR` in a terminal; nonterminal edits need --from. Add can ask for transport/URL/program in a terminal. Prompts/editor use stderr and leave stdout JSON. Source changes while input/editor is open are rejected.

All commands honor --config and normal config precedence. Add can create a missing file at onboarding's explicit creation path; the receipt names it. Read/edit/remove never create an explicitly missing file. Ordinary block YAML preserves other nodes, comments, order and unexpanded environment references. Unsupported flow/anchor/complex layouts and link-backed write targets are refused with $EDITOR guidance. The shared publication lock protects the exact-source check through atomic replace, retaining permissions; arbitrary external writers must cooperate with that lock during publication. After a post-rename directory-sync failure, inspect saved state before retrying.

Saving validates a private candidate (including static MCP/host and policy rules); unset variables may use validation placeholders. It neither installs nor starts MCP processes, contacts upstreams, reloads nor restarts. Receipts say saved_only/running unknown and explain startup restart requirements. Use `bro status` for target-owned evidence and explicitly restart after active work; route reload does not replace native MCP bindings. Check/tools/gateway/relay retain existing behavior. Hidden `mcp search QUERY` remains separate registry discovery. Versioned skill installation is not implemented by these commands.
