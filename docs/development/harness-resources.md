# Native BRO project instructions, MCP and skills

> Historical first-party UI interfaces described here have been removed.
> Current commands and execution entry points are documented in the
> [CLI reference](../CLI.md).

The host supplies static daemon configuration to the native `ThreadService`.
An admitted execution pass discovers its resources; cold inspection does not.
Native MCP uses execution-owned connections independent of the gateway pool.
Model calls continue through the application's routed pipeline. The runtime
retains one queue, durable store and execution owner rather than creating a
resource-specific scheduler.

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
profile follows its existing approval policy. Read-only Threads expose no MCP
tools; only explicitly granted native skill sources may connect for metadata
and source-bound reads. Upstream read-only annotations do not grant local
permission. An allowed workspace is not a filesystem sandbox for a
configured MCP server; stdio launches trusted operator-selected code, and HTTP
uses the operator-selected server credentials.

Create, cold directory, history, open/load and observer reconnect do not read
project instructions, discover resources or spawn processes. An active pass
discovers once, persists its frozen inventory before any model request, and
closes connections before settlement.
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

New roots and appended roots use runtime format **7**. Formats **2/3/4/5/6** remain
readable; a new append upgrades the root envelope in the same version-fenced
transaction, without rewriting its history or identities. An older binary refuses
format 7 before decoding new instruction/resource/close facts. Formats 0/1 and future versions remain
unsupported. There is no schema migration or automatic lost-owner retirement.
Local protocol is v19. Inventories still use optional snapshot/context fields;
the new version prevents older task-run clients from auto-approving MCP skill
requests as ordinary commands. HTTP v2 advertises `explicit_skill_consent` and
requires `skill_consent: true` on an answer to `mcp_skill.activate`.

## Workspace project instructions

The first active execution of a live Thread loads a startup instruction snapshot.
The host supplies the global root: the daemon uses its BitRouter home, normally
`~/.bitrouter`. Global discovery selects the first nonempty `AGENTS.override.md`
or `AGENTS.md`. Project discovery walks from the nearest Git root through the
selected workspace/working directory; without a Git root it checks only that
directory. Each level selects at most one regular file in this order:
`AGENTS.override.md`, `AGENTS.md`, then configured fallback filenames. Empty
project files are skipped after selection. Deeper directories are not scanned.

Instruction discovery has its own host-authorized read ceiling. Authenticated
local workspace registration permits ancestor instruction reads to the nearest
Git root; remote discovery stays within granted roots. This never expands native
tool access. Symlinks must resolve inside the respective project/global read
root. The project chain shares a 32 KiB default budget, host configurable up to
64 KiB; global content is bounded separately at 64 KiB. Excess bytes are truncated
with persisted warnings and UTF-8 is decoded with replacement characters.
Serialized startup snapshots are bounded at 512 KiB, including source metadata.

Markdown bodies enter a distinct **user message** before the first task input,
using the `AGENTS.md instructions for ...` wrapper. No frontmatter or prescribed
headings are required. System instructions define scope, deeper-rule precedence,
direct-prompt precedence and require the model to inspect applicable instructions
before operating in deeper directories. Existing `read` results carry those
files into ordinary durable tool history. Project content grants no tools,
approval changes or additional runtime bounds. All content counts toward the
existing model-context bound; a truncated project budget does not bypass it.

`InstructionContext` facts commit source/digest references, loaded body and the
optional model-visible message before sampling. They use the same Thread owner
and commit barrier as other context facts; public history projects only context
advancement, never a synthetic user submission. Empty snapshots also commit.
The live Thread caches discovery across Turns, even after files change. Reopening
under the same server owner retains that cache. A new Thread discovers again.
After server restart, an authorized continuation re-discovers only at a settled
model boundary and appends an explicit replacement/removal message if needed.
Already settled outcomes and cold reads load no instruction files. MCP catalogs
retain independent frozen-inventory checks, and known effects are never replayed.

Regressions inspect actual coding/read-only model requests, global/ancestor order,
override/fallback selection, bounded decoding, symlink/read ceilings, live-Thread
caching and durable refresh. Recovery prefixes preserve known MCP results while
adding, replacing or removing startup instructions; changed MCP bindings still
prevent sampling. SQLite reopen preserves startup facts without rereading files.

## Skills discovery

The CLI also manages BitRouter-owned immutable bundles through
`bro skills install/show/update/uninstall`; see [CLI skills](../CLI.md#skills).
G09 now supplies a read-only adapter over G07's same aliases, retained receipts
and immutable version files. Native resource inventory records active managed
entries and per-library revisions, including empty catalogs. A version reader
keeps scope/installation identity and the exact whole-bundle receipt; updates
or uninstall do not substitute a new version. The managed library is separate
from the compatibility discovery below. Native Core now selects managed versions
through its configured decision backend and freezes original receipts, reasons,
budgets and verified instructions in the existing WorkUnit/decision ledger before
context/model preparation. Each child evaluates its own candidates; explicit
user constraints remain inherited. MCP roots use explicit consent and a verified
per-Turn cache. Explicit task include/exclude selectors are supported, including
direct URI metadata lookup independently of listing. Native `skill_read` loads
only files committed by the selected original receipt/entry. It records a metadata
tool receipt and persists verified UTF-8 data in that Turn's WorkUnit; subsequent
model inputs receive a protected user data frame with parent/file provenance.
No file body enters ordinary tool history or automatically activates a nested
skill. `native_skill_files_v1` gates these cache/budget facts during restoration;
legacy Turns without the feature/frozen budget receive no new tool. File data
counts against the frozen Turn allowance and final model/context admission,
without truncation. Local fixtures cover active update/uninstall and original
SQLite restoration; lost owners retain the existing recovery block.

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
system prompt. Native managed selection and verified root/support delivery use
the path above; compatibility discovery alone does not activate material.
Plugin/namespace recursion and selection/value optimization are separate work.

G09's native loading primitives additionally admit MCP entries only under
`skills.mcp_sources.<configuration-key>`. Each source needs exact absolute
workspace roots and a matching synthetic-local, authenticated key-ID or user-ID
grant. These grants do not replace the existing execution caller/workspace and
tool permissions. Removed configurations cannot authorize old caches. Source policy and
transport changes are startup-owned; apply them with an explicit restart.

The host keeps the complete entry, original URI, host-assigned configuration ID,
configuration digest and caller/workspace scope. A live connection must
declare Resources and Skills. Listing and direct lookup fetch no files. Held
manifest reads use the same source, verify raw byte size/hash, and compare the
complete root frontmatter. Supplied cached bytes undergo the same checks on
every access. Generic URI/IRI syntax permits rootless, authority-based and
absolute paths without URL normalization or authority resolution. Query and
fragment components remain part of each exact resource identity; only the path
determines directory containment. Filesystem-relative reads match the original
held manifest; ambiguous decoded paths require an exact URI. Nested SKILL.md
reads remain supporting data. Dynamic
entries are excluded with reasons; direct dynamic loading is rejected.

Native Core can select held entries and request explicit per-skill consent through
the original checkpoint ledger. Native Threads use their pending/answer controls;
managed CLI uses `task managed-status` and `task managed-answer` with the original
boot/input/consent identity. Ordinary headless command approval does not approve a
skill. Root reads follow the consent ACK; exact entry and verified root bytes
enter source-tagged user data, rather than system instructions. The frame counts
toward context budgets and is not automatically shared with unconsenting children.
Tool grants remain unchanged. The original WorkUnit retains ACKed root bytes;
later model steps and authenticated Core restoration reuse that exact cache
after current caller/workspace/configuration and raw-byte validation. No cache
directory participates in filesystem skill discovery. A resource-only source
can be offline without supplying new candidates; missing cache bytes still need
the original server. Connections supplying tools or server instructions retain
their execution-catalog guards. A new live connection without Resources/Skills
cannot authorize cache use. Supporting-file cache/data uses the same source and
approval checks, with exact file size/hash and protected evidence validation.
Local CLI, model-input and restored-journal fixtures cover these native gates.
Daemon restart does not authorize lost-owner takeover or task replay. Supervised
Codex/Claude support separately reported local prompt-instruction delivery;
required MCP-origin external delivery is unsupported. Vendor history/token
admission and real provider/adapter/remote-MCP conformance remain unverified.

`bro skills explain` queries original native Thread/Turn records or a managed
session/input in the selected database. It uses bounded store pages, existing
checkpoint/projection verification and caller ownership for native Threads; no
App, resources, daemon, ownership claim or restoration is needed. It reports
source/version/entry, constraints, reasons, policy, consent, verified-byte
commitments and exact prepared SDK inputs separately from available execution
observations. Missing historical attempt details stay unknown. Workspace or source
file deletion does not replace the original evidence; lost owners remain unreleased.

Load errors are checkpoint facts. An optional root that has never activated can
be excluded with a retained cause; it contributes no model instructions. A
required root, or an already loaded version that can no longer be verified,
fails the original accepted work without replacing its binding. Permission,
signal, context and steering changes cancel stale reads before their bytes can
be committed. An unacknowledged failure cannot authorize a model fallback.

## Bounds and verification

MCP accepts at most 32 servers, 256 tools and 1 MiB of serialized declarations;
server instructions are bounded to 64 KiB each. Native inventory, including skill
metadata, is capped at 256 KiB, so this tighter bound can reject a catalog accepted
by MCP-only diagnostics. MCP skill entries are capped separately at 2 MiB of
metadata, listings at 16 MiB and 256 skills per source. The supported atomic
manifest baseline is 512 files/16 MiB, with per-file host/context read budgets;
content is never fetched to satisfy metadata listing. Connection/list requests have a 30-second bound and
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

## AGENTS.md working tree validation, 2026-10-06

The AGENTS.md implementation was validated locally in a working tree based on
`b9527673da3ef9df751be192210ab72725c39208`. Validation used the current working
tree, which also retains pre-existing `actions/route.rs` and `commands.rs` edits.
It ran on macOS arm64/Rust 1.97.0 with `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0`.

| Check | Result |
| --- | --- |
| `cargo nextest run --all-features --no-fail-fast` | 3,746 passed, 22 skipped; no reported leaks |
| `cargo test --all-features --doc` | 5 passed, 1 ignored |
| `cargo clippy --all-features --all-targets -- -D warnings` | Passed |
| `cargo fmt -- --check` and `git diff --check` | Passed |
| Plugin JSON, changed Markdown relative links/fences and skill size | Passed; `SKILL.md` remains 199 lines |

The startup and nested-read tests inspect actual model requests and committed
tool history, including read-only execution, local/remote read ceilings, empty
snapshots, same-owner unload/reload and cumulative project truncation. Corrupt
source/message/version records and instruction updates during an unsettled
model step block recovery. Stopped-source prefixes preserve the known MCP result
while adding/replacing/removing instructions; SQLite cold reopen reads no
instruction files.
These are local fixture contracts, not proof that a credentialed model follows
every nested rule or that Windows/Linux, hosted CI or production has been checked.

Logs are under `/tmp/bitrouter-agents-validation-nextest.log`,
`/tmp/bitrouter-agents-refactor-clippy-final.log` and
`/tmp/bitrouter-agents-refactor-doctest.log`. Existing macOS unwind-section and
`proc-macro-error2` future-compatibility notices remain.

## MCP and skills source and validation, 2026-10-05

Implementation source: `321ccaad5fe69f7adb39e0230a11f6e1851ff84d`, based on
PR #945 head `fb243ee5e18dde10aaec3cfccf96f6b6a7e34bc0`. PR #956 is not a
dependency. Validation ran locally on macOS arm64 with Rust 1.97.0,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0` and
`CARGO_PROFILE_TEST_DEBUG=0`.

| Check | Result |
| --- | --- |
| `cargo nextest run --all-features` | 3,733 passed, 22 skipped; no reported leaks; run `4c607e51-8ae7-4a97-9ae8-948e0604f1ac` |
| `cargo test --all-features --doc` | 5 passed, 1 ignored |
| `cargo clippy --all-features --all-targets -- -D warnings` | Passed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps` | Passed |
| `cargo fmt -- --check` and `git diff --check` | Passed |
| Three plugin JSON manifests; changed Markdown relative links/fences | Passed; shippable `SKILL.md` remains 199 lines |

The native regressions verify inventory commit before model sampling, denied and
read-only calls, native collisions and invalid pagination, observed in-flight
cancellation, tool-list invalidation and inventory commit failure. Real stdio
runs in the workspace; both its parent and a spawned descendant are gone before
Turn completion. Cancelling an observed stalled initialization joins the process
before the database owner is marked stopped. A stopped-source checkpoint prefix
reuses the completed MCP result and rejects changed connection bindings. The
SQLite regression upgrades an existing format-2 envelope on append, reopens the
stored inventory/results after changing the skill body and observes no new MCP
requests. MCP check also exercises legacy initialization and configured modern
server discovery using the same client.

During implementation, initialization cancellation initially lacked a joinable
cleanup owner; a later redundant-kill error also incorrectly blocked a cancelled
Turn. The final owner retains the child across future cancellation and proves
scope exit after waiting. Both cancellation and descendant-exit regressions pass.
Clippy found a function after the test module and an unnecessary cloned slice;
both were corrected before the final run. The macOS linker still emits its
existing large unwind-section warning, and Cargo reports a future-compatibility
notice for `proc-macro-error2`; neither prevented the recorded checks.

Run logs were retained locally under `/tmp/pr945-321ccaad-{nextest,doctest,rustdoc}.log`
and `/tmp/pr945-harness-clippy-complete.log`; these are reproduction provenance,
not committed artifact downloads. Hosted CI, Windows/Linux process behavior,
credentialed MCP/provider usage and abrupt-owner recovery are separate gates.
Skills activation/material delivery, Core context/value selection and native
multi-agent scheduling remain subsequent work. Native ACP uses this Harness
unchanged; see [native ACP format](../specs/service-api/README.md#native-acp-extensions).
