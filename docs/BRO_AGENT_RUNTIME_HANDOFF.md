# BRO runtime to orchestrator core migration handoff

Updated: **2026-10-01**. Status: **source-verified migration notes; no new core
API or production harness integration delivered by this document**.

**Current sequencing:** the user supplied the core engineering spec and confirmed
standalone runtime/session completion with independent crate validation first.
CLI/HTTP/ACP delivery and core integration are subsequent tracks.
The user also deferred lost-owner/effect investigation and operator proof to
host integration; unknown records remain blocked in the standalone runtime.
The source availability/blocker audit below records the earlier investigation; the missing
spec is no longer a current blocker. Current source:
[core spec v1.0 at a93ea456](https://github.com/bitrouter/bitrouter/blob/a93ea456f6eb69bcc17d9cf2fac63808ae81f20d/docs/ORCHESTRATOR_CORE_SPEC.md).

Product [003 runtime v0.2](/Users/kelsen/Documents/bitrouter/product-engineering/bit-router-orchestrator/003-BRO-Agent-Runtime-Design-and-Implementation.md)
now defers conflicting ownership, interfaces and subsequent stages to
[004 core/harness v1.0](/Users/kelsen/Documents/bitrouter/product-engineering/bit-router-orchestrator/004-Orchestrator-Core-and-Harness-Contract.md).
The [runtime spec](BRO_AGENT_RUNTIME_SPEC.md) and
[implementation ledger](BRO_AGENT_RUNTIME_IMPLEMENTATION.md) retain the earlier
execution requirements and evidence. They do not establish the new C0–C6 core
contract or its completion.

## Authority and available sources

Orchestrator 004 assigns agent scheduling, active context and model/context
routing to core. Harness owns workspace tool execution, durable workflow/session
authority, interaction and edge facts. One session has one effective core owner
and one durable harness authority. The same API is intended for in-process,
local-service and remote-core deployments; crate placement alone does not prove
that separation.

004 is distinct from the older **router** document 004 about the first-party
orchestrator. References to the two must identify the document, not just “004”.

004's original `/Users/archer/.../ORCHESTRATOR_CORE_SPEC.md` path is not present
on this host. The user subsequently supplied the `codex/orchestrator-core` branch;
its spec and implementation record were inspected at
`a93ea456f6eb69bcc17d9cf2fac63808ae81f20d`. The earlier 404/path investigation below
is historical, not a current blocker. Keep its DTOs/defaults/A01–A23 on the
later core integration track. The user explicitly chose to finish the existing
standalone runtime first; source availability does not authorize migration now.

## Existing implementation and destination responsibilities

These are current migration inputs, not a claim that the boundary has already
been implemented. Preserve their regressions and recorded limitations.

| Current source | Verified responsibility today | Responsibility under product 004; remaining work |
| --- | --- | --- |
| [`agent.rs`](../crates/bitrouter-orchestrator/src/agent.rs), [`context.rs`](../crates/bitrouter-orchestrator/src/context.rs) | Single native Turn loop, SDK requests, legal context, stable Items and commit barriers | Core execution/context input; introduce the verified session/run/agent/turn/attempt contracts without a second loop |
| [`service.rs`](../crates/bitrouter-orchestrator/src/service.rs), [`service/threads.rs`](../crates/bitrouter-orchestrator/src/service/threads.rs), [`service/steering.rs`](../crates/bitrouter-orchestrator/src/service/steering.rs) | One owner coordinates Thread/FIFO/steering, approvals, workers and settlement | Separate core scheduling transitions from harness permissions/execution and durable acknowledgement; preserve ordered control semantics |
| [`store.rs`](../crates/bitrouter-orchestrator/src/store.rs), [`agent_store.rs`](../apps/bitrouter/src/agent_store.rs), migrations 000022–000025 | Transactional facts/keys/root index and owner/version fencing; database backend is app-owned | Harness persistence input; current direct store calls are not a remote proposal/digest/ACK protocol |
| [`tools.rs`](../crates/bitrouter-orchestrator/src/tools.rs), [`service/workspace.rs`](../crates/bitrouter-orchestrator/src/service/workspace.rs) | Local tool validation/execution, read permits, exclusive barriers, process cleanup and workspace sidecars | Harness execution/resource enforcement input; a durable intent or checkpoint proposal alone must not launch a tool |
| [`service/ownership.rs`](../crates/bitrouter-orchestrator/src/service/ownership.rs) | One owner for a configured store; clean stop can transfer it; lost/unfenced ownership blocks execution | Preserve existing safety until per-session harness-issued epochs and exact owner checks are implemented and verified; do not equate these two contracts |
| [`service/recovery.rs`](../crates/bitrouter-orchestrator/src/service/recovery.rs), [`service/startup.rs`](../crates/bitrouter-orchestrator/src/service/startup.rs) | Bounded native/legacy reconstruction/discovery; explicit terminal conversion and safe same-Turn checkpoint recovery after stopped proof; loading alone stays blocked | Preserve recovery gates and exact ACK behavior when adapting ownership; lost-owner/effect investigation and new core agent/run contracts remain separate |
| [`service/observation.rs`](../crates/bitrouter-orchestrator/src/service/observation.rs), [`thread.rs`](../crates/bitrouter-orchestrator/src/thread.rs) | Durable public projection and bounded live observation; UI disconnect detaches | Retain UI semantics while distinguishing observer detachment from durable harness loss, which must stop dependent new execution |
| [`agent_local.rs`](../apps/bitrouter/src/agent_local.rs), [`agent_api.rs`](../apps/bitrouter/src/agent_api.rs), [`native_code.rs`](../apps/bitrouter/src/native_code.rs) | Existing local v13 and HTTP one-shot Task delivery over the same service | Compatibility inputs; managed Responses, harness WebSocket, continuous native/HTTP controls and inbound native ACP still need implementation and independent acceptance |

App composition and SDK model execution remain in their established dependency
direction. Product 004 does not require a new `bitrouter-core` crate or make SDK
depend on orchestrator. Existing nested `subagent` completion and external ACP
controllers do not provide the required persistent native agent scheduler.

## Correctness that survives the migration

Preserve identity, admission deduplication, complete response/call/result facts,
queue pause, steering/cancel, approvals and cumulative budgets. Stable invocation
identities remain separate from provider-local call IDs. Missing historical Item
identities remain invalid; migration must not silently generate replacements.

Core may advance execution only after the harness acknowledges the exact durable
batch and revision. Lost ACK recovery reads the committed head and uses the
original operation identity. A proposal is distinct from the post-commit command
that authorizes tool execution. Existing direct store acknowledgements establish
the local barrier; they do not prove the new channel's digest/artifact checks or
idempotent retransmission behavior.

Known complete tool results may rebuild context without execution. Unknown
shell/write effects require harness investigation and confirmed old-execution
termination or isolation. A new epoch, stopped database owner, repaired message
pairing or idle filesystem marker does not by itself resolve every in-flight
provider/tool window. Partial model output remains display evidence, with unknown
usage retained. Never turn missing usage or disconnected active time into zero.

The model-input view and client history stay distinct. Routing must preserve
applicable instructions, permissions, acceptance conditions and legal call/result
pairing. Referenced material must be fetched and validated before use; a task or
context reference alone is not loaded context. Workspace revision, context
revision and provider continuation are separate facts. One worker's assessment
does not become verified evidence without provenance.

Cold snapshots use the current serving epoch. Reconstruction must consume later
committed writer checkpoints, while historical public events keep their original
epochs. Loading creates no runner, approval sender or effect and never resumes
the FIFO. The later-writer checkpoint regression is tracked in the runtime ledger.

## Next concrete implementation decisions

1. After standalone runtime acceptance, discuss the user's chosen core integration
   scope and reconcile the inspected schemas, acceptance IDs and defaults. Keep the
   original runtime ledger as evidence, without renaming R phases to C phases.
2. Define the consumed core/harness seam around the existing commit and tool
   launch barriers. Use the verified contract's batch identity, digest, sequence,
   revision, artifacts and epoch checks; avoid an unused interface scaffold.
3. Adapt the existing app database and local tools as the production harness,
   while retaining one scheduler and the shared SDK execution pipeline. Exercise
   permission refusal, stale commands, commit failure and lost ACK before adding
   dependent scheduling or transport paths.
4. Introduce native agent/run/context decisions through that same owner. Map
   existing Thread/Turn identity explicitly rather than treating it as a proved
   alias for every new identity domain. Preserve old Task compatibility without
   duplicate authority or effect replay.
5. Implement remaining recovery and transport work under the confirmed ownership
   contract. Prove actual process-fault windows, production harness reconnect,
   real providers/clients, platform behavior and quotas separately from mocks.

These decisions do not remove unfinished full-product R4/R5/R6 requirements.
The standalone runtime now has terminal legacy conversion and safe checkpoint
continuation; lost-owner/effect operator resolution is deferred to host work.
The new managed core API, persistent native sub-agents and
joint executor/context/model selection are also unimplemented. Passing the old
suite cannot establish either complete MVP.

## Implementation blocker audit

**Historical audit, superseded by the source and sequencing decisions above.**
The following records the earlier unavailable-file investigation and the code at
that point. It is not the current implementation status or work authorization.

Revalidated **2026-10-01** after three consecutive goal turns with the same
unavailable engineering-file prerequisite. The earlier two turns made independent
progress (epoch projection and legacy read compatibility). Those changes do not
provide the missing core/harness contract or complete the implementation goal.

The exact `/Users/archer/.../ORCHESTRATOR_CORE_SPEC.md` reference still does not
exist on this host. A renewed scan of the local Documents/worktree roots found no
copy. Current GitHub branch inventory still exposes the same three relevant refs
listed above; renewed file queries at `main` and each of them returned 404. This
does not prove absence from every private checkout or arbitrary branch. No
accessible current core engineering spec was obtained.

| Remaining requirement | Current authoritative evidence | Missing prerequisite |
| --- | --- | --- |
| C0 typed harness protocol, capabilities, errors, exact durable batch acknowledgement | Product 004 sections 4–6 describe identities, digest/revision checks and one pending batch; current `ExecutionStore` supplies local owner/version CAS | Referenced engineering schemas, defaults, error contract and A01–A23 acceptance details |
| Durable legacy conversion, old-execution/effect resolution and same-Turn continuation | `load_thread` is read-only; recovered records keep cancelled tokens and no approval sender; no conversion/resolution/continuation method exists | Reconciled session authority, conversion/recovery transitions and confirmed effect/commit protocol; the read adapter supplies none of these permissions |
| C2 persistent agents and C3 joint executor/context/model selection | Current source provides Thread/Turn state and a fixed-model native loop; existing nested completion is not a persistent native agent scheduler | Engineering identity, dispatcher, context manifest and routing decision contracts |
| C5 managed Responses, authenticated harness channel and shared client delivery | No native managed orchestrator channel/capabilities routes or persistent collaboration dispatcher were found in app/orchestrator source; existing Task delivery remains one-shot | Verified managed extension/channel schemas and ownership/compatibility mapping |
| C6 and remaining R6 end-to-end acceptance | Latest recorded local suite: 3667 passed, 22 skipped; Clippy/fmt/doc checks passed. Ledger still lacks real provider/harness/client, platform and native process-fault continuation proof | Implemented target integration and its actual acceptance contract; old local checks cannot prove these requirements |

The goal remains incomplete. Continuing ownership-changing or new interface work
from the older runtime schema would choose a different contract from the current
product authority. Required input is an accessible engineering spec/revision, or
an explicit user direction to finish the original 003 v0.2 R4–R6 scope before
the core/harness migration. Preserve the implementation and existing checks;
do not manufacture replacement C schemas, relabel the R ledger, or restart the
production daemon to bypass this prerequisite.
