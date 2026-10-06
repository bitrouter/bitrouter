# BRO core and harness migration boundary

Updated: 2026-10-07. The explicit native managed entry below implements a bounded
core/harness integration. The existing standalone runtime contract is
specified in [BRO_AGENT_RUNTIME_SPEC.md](BRO_AGENT_RUNTIME_SPEC.md); validation
is indexed in [BRO_AGENT_RUNTIME_IMPLEMENTATION.md](BRO_AGENT_RUNTIME_IMPLEMENTATION.md).

## Target contract

The inspected [core engineering spec v1.0 at `a93ea456`](https://github.com/bitrouter/bitrouter/blob/a93ea456f6eb69bcc17d9cf2fac63808ae81f20d/docs/ORCHESTRATOR_CORE_SPEC.md)
defines the later core/harness seam. Core owns live scheduling, checkpoint/recovery
semantics and model/context decisions; Harness is the durable storage and exact
ACK authority, executes workspace tools and supplies permissions/edge signals.
One session has one effective core owner and one durable Harness authority.

This target does not make direct standalone store calls equivalent to the managed
proposal/digest/ACK protocol. Its availability does not establish that the App's
production database or tools implement that protocol. Other branch acceptance
and A01–A23 remain attributable to those sources, not PR #945.

## Current migration inputs

| Current component | Preserve during adaptation |
| --- | --- |
| `agent.rs`, `context.rs` | One loop, legal context, stable identities, cumulative limits and model-request barriers |
| `ThreadService` and `service/threads.rs` | One admission/FIFO/control authority and commit gate per Thread |
| `store.rs`, app `agent_store.rs`, migrations 000022–000026 | Atomic facts/keys/public events, root format 4 (readable formats 2/3), bounded reads and owner/version fences |
| `tools.rs`, `service/workspace.rs` | Grant checks, bounded reads, exclusive effects, tracked process cleanup and exclusion markers |
| `service/recovery.rs`, `ownership.rs`, `startup.rs` | Exact source/cursor validation, stopped-owner proof, conservative unknown-effect/accounting handling and complete discovery |
| `service/observation.rs`, `thread.rs` | Post-commit public projection, bounded history and observer detachment without cancellation |
| App local/HTTP/native clients | Local v15, HTTP v2, shared Thread/Turn operations and continuous native context |

Core identity domains cannot be assumed to alias Thread/Turn/Item without an
explicit mapping. Legacy standalone Task APIs/conversion are removed; migration
must not restore duplicate execution paths to accommodate the old documents.
SDK remains the routed provider pipeline and does not depend on orchestrator.
The existing local clients and external ACP controllers do not constitute an
authenticated managed Harness channel or persistent native sub-agent scheduler.

## Migration requirements and evidence boundary

Adapt the consumed seam around existing durable commit and tool-launch barriers.
Core advances only after acknowledgement of the exact durable batch/revision;
a proposal is distinct from the post-commit command permitting execution. Verify
operation identity, digest, sequence, revision, artifacts and owner epoch.
Lost ACKs require reading/adopting the exact original batch, not replaying effects.

Retain known results, original identities, ordered steering, FIFO pause, approval
binding and budgets. Unknown shell/write effects require old-execution termination
or isolation plus effect investigation. A new epoch, idle marker, stopped database
owner or repaired message pairing alone does not resolve every provider/tool window.
Observer detachment differs from losing the durable Harness authority.

Before claiming integration, prove permission refusal, stale commands, commit
failure, lost ACK, owner/channel loss and context/artifact validation against the
actual host backend. Production persistence, authenticated remote Harness,
credentialed providers, platform/stress and full managed acceptance remain
separate gates. This documentation consolidation implements none of them.

## Historical investigation

The earlier unavailable-file audit and product-document sequencing discussion
are preserved in [the prior handoff](https://github.com/bitrouter/bitrouter/blob/65555b126e8d14982b2a7c977b618d545cd8f5b7/docs/BRO_AGENT_RUNTIME_HANDOFF.md).
They are historical context, not a current prerequisite or authorization.

## Current resource integration

MCP connections and skills discovery are now owned by the standalone harness
within this runtime; see [BRO harness resources](BRO_HARNESS_RESOURCES.md).
`bro task managed` now adapts those resources through `harness::managed`, with
CoreSession as its sole model/sub-agent scheduler. `NativePort` commits exact
checkpoint/ACK bytes, artifacts and tool-start fences through a database CAS;
native execution retains the shared WorkspaceFence and persists starts/results.
The CLI reuses the configured provider pipeline and database, but opens no HTTP
listener. Legacy `task run` and `code` continue through ThreadService.

The bounded continuation contract requires a durably released prior session and
the same workspace/mode. Abrupt process loss, remote harness authentication,
physical storage reservation and running-owner recovery remain separate gates.
`apps/bitrouter/tests/managed_native.rs` exercises real SQLite migrations and
workspace tools with scripted model replies; provider evidence must be reported
separately. This entry does not convert legacy Thread records into core sessions.


The native managed CLI publishes skill metadata and optional MCP instruction
references, but does not automatically select these optional materials for model
context. Library callers can select their IDs through `TaskInput.required_materials`;
AGENTS.md is always required. Automatic skill selection remains follow-up work.
