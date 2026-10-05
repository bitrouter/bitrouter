# Native BRO MCP and skills resources

This is the resource extension of PR #945's production `ThreadService`. The app
wires static daemon configuration in `host.rs`; `Agent` discovers resources while
preparing an admitted execution pass. MCP is a direct harness client, independent
of the SDK's gateway pool. Model generation continues through the routed SDK
pipeline. This change does not import PR #956's Core or introduce another loop,
queue, database owner or scheduler.

## Configuration and lifecycle

Coding Threads use the daemon's configured `mcp_servers` and
`mcp.upstream_protocol`. Names are taken from the configuration map keys and
sorted. Missing prefixes become `<server>__`; final names must fit the model-tool
identifier grammar (1–64 ASCII letters, digits, underscores or hyphens).
Collisions, including with the six native tools, fail before sampling.

Configuration is bound at daemon startup. Reloading inference routing does not
replace native MCP credentials, transports or roots; restart the daemon to change
these bindings. MCP credentials are available only on the execution host and
are not copied into public inventory or prompts. The binding digest covers the
selected configuration and workspace; it is not an authorization grant.

The existing workspace/caller and permission-profile checks apply. `Ask` requests
an identified approval for MCP effects; an explicitly granted `AllowEffects`
profile follows its existing approval policy. Read-only Threads never connect to
MCP servers or advertise their tools: upstream read-only annotations do not grant
local permission. An allowed workspace is not a filesystem sandbox for a
configured MCP server; stdio launches trusted operator-selected code, and HTTP
uses the operator-selected server credentials.

Create, cold directory, history, open/load and observer reconnect do not discover
resources or spawn processes. An active pass discovers once, persists its frozen
inventory before any model request, and closes connections before settlement.
Stdio runs in the selected canonical workspace. The stdio transport owns a Unix
process group or Windows Job Object, stops it and waits during close. Ownership
survives interrupted initialization so cancellation can join the spawned process;
failed or unfinished cleanup cannot establish a cleanup receipt. The
service retains an active database owner when cleanup is unconfirmed. Actual
Windows process behavior still needs its own platform validation.

## Execution and recovery

MCP declarations join `read/glob/grep/write/edit/shell` in the actual native
model request. Calls use the same identified approval, tool intent commit, launch
fence, worker budget and result commit as exclusive native effects. MCP calls are
not promoted into the native parallel read group. A complete result retains its
JSON content, including structured/multimodal fields; `isError` selects an error
output while its effect is known/completed. There is no image-content conversion.

Dispatch interruption, cancellation after dispatch, transport loss, incomplete
or task responses and an oversized result retain `EffectStatus::Unknown`. An
uncertain effect blocks continuation; the harness never retries `tools/call`.
Tool-list notifications or a closed connection invalidate the catalog before
later model requests/dispatch. A new pass obtains a new connection/catalog.

A safe, explicitly accepted continuation reconnects and compares the complete
frozen inventory and binding digest. Changed configuration, schemas, instructions
or skills refuse further sampling; an old Turn without an inventory cannot gain
MCP tools during continuation. Committed results are reused, not executed again.
Reconstructing or completing an already settled outcome does not reconnect or
replace its original inventory, even when files/configuration have since changed.

New roots and appended roots use runtime format **3**. Format **2** remains
readable; a new append upgrades the root envelope in the same version-fenced
transaction, without rewriting its history or identities. An older binary refuses
format 3 before decoding new resource facts. Formats 0/1 and future versions remain
unsupported. There is no schema migration or automatic lost-owner retirement.
Local protocol remains v15: inventories use optional fields in existing snapshot
and context-advanced messages; no new public message variant is introduced.

## Skills discovery

The orchestrator owns the shared parser and discovery implementation used by
`bro skills list` and `skills init`. Each active pass includes its workspace and
host-selected user roots: `$HOME/.agents/skills`, `$HOME/.codex/skills`,
`$HOME/.claude/skills`, plus `$CODEX_HOME/skills` when set. Windows uses
`USERPROFILE` for the home directory. An embedding selects its own roots through
`HarnessConfig`; API callers cannot supply these roots.

Under each root, discovery considers its direct `SKILL.md`, immediate skill
children, and immediate children under `skills/`, `.claude/skills/`,
`.agents/skills/` and `.codex/skills/`. Missing roots are skipped. Regular files
are required; symlinks beneath the selected root are skipped. Invalid and
oversized files are reported rather than advertised as usable skills. Paths
must be UTF-8; canonical paths are deduplicated, and duplicate names from
different roots retain distinct path-derived material IDs.

The inventory exposes name, description, source and a content-hashed version.
It contains no skill bodies. Discovery does not activate skills, execute scripts,
expand frontmatter `allowed-tools`, or inject skills/MCP instructions into the
system prompt. Material-body delivery, plugin/namespace recursion and Core
selection/value optimization remain subsequent work.

## Bounds and verification

MCP accepts at most 32 servers, 256 tools and 1 MiB of serialized declarations;
server instructions are bounded to 64 KiB each. Native inventory, including skill
metadata, is capped at 256 KiB, so this tighter bound can reject a catalog accepted
by MCP-only diagnostics. Connection/list requests have a 30-second bound and
per-server discovery has a total 30-second bound; preparation also respects the
remaining Turn deadline/cancellation. Calls have a 120-second total bound and
32 KiB retained output allowance; cancellation cleanup and close are bounded.

Skills accept at most 32 roots (including the workspace), 4096 directory entries
per root and 256 unique discovered entries. A file is at most 256 KiB and valid
skill source bytes total at most 16 MiB. These are discovery/retention bounds, not
proof of physical memory bounds within parsers, rmcp or HTTP transports.

Regressions use actual ThreadService execution, its durable commit/approval gates,
HTTP and real stdio. They cover denied/read-only calls, pagination errors, native
collisions, observed cancellation, catalog invalidation, joined cleanup, inventory
commit failure and exact checkpoint continuation without replay. The app SQLite
reopen regression retains inventories/results, upgrades a format-2 envelope on
append, and proves cold loading does not reconnect after a skill file changes.
Scripted provider output and stopped-source prefix tests are local contract
proof; they do not establish credentialed-provider or abrupt-owner recovery proof.
