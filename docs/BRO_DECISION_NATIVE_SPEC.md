# BRO decision-native context routing

Implementation branch: `codex/decision-native`, based on orchestrator core
`545ecf07` (#956, stacked on #945). This document tracks implementation, not
benchmark results. Benchmark experiments are outside this change.

## Contract

BRO owns durable tasks and immutable evidence. Each execution compiles a
task-specific context view and freezes a model binding. Omitting evidence from
a view never deletes its source. A worker receives a view of shared evidence;
its completion and artifact references return to its parent without copying
the entire worker transcript. Compaction and worker context construction use
the same compiler.

Decision models answer typed, bounded semantic questions. Generation models
write code, task descriptions and derived summaries. Code enforces authority,
mandatory instructions, tool exchange integrity, source freshness, context
capacity, manual model/effort overrides and workspace effect barriers. Decision
output cannot grant permissions or discard required material.

TypeSafe is an independent SDK protocol using `POST /v1/systemone`, with
Choice, Score and Noul questions, response validation, cancellation, bounded
I/O and observed usage. Official contract:
<https://api.typesafe.ai/openapi.json> and
<https://docs.typesafe.ai/primitives/choice>.

Decision intent must be durably acknowledged before dispatch. The result,
usage and source revision must be acknowledged before applying a plan. Recovery
reuses committed decisions; interrupted attempts retain unknown cost. Steering
and changed source revisions invalidate a pending selection. Failed, ambiguous
or invalid decisions select conservative context if it fits, otherwise report
an explicit capacity failure.

The native Thread/Turn/Item service adapts the existing Core scheduler to
workspace tools, resources, approval, persistence and user-facing events. The
decision-native path must be reachable through native BRO, not only through the
managed remote protocol.

## Implementation

The SDK exposes independent Choice, Score and Noul protocols with bounded,
cancellable TypeSafe HTTP execution, strict response validation, explicit usage
and optional operator pricing. App assembly reads credentials from the configured
environment variable and injects the runtime into Core; credentials never enter
checkpoints.

All native BRO Agent execution uses the Core adapter, including configurations
without a decision backend. Instruction discovery, resources, approvals, workspace
ownership, tool concurrency and cancellation remain native host responsibilities.
The adapter commits exact Core facts through the Thread owner, projects stable
Thread/Turn/Item identities and validates those projections during cold replay.
The former native batch/stream execution loop has been removed.

Core stores immutable evidence groups and task-owned inventories. Each model
step records its source revision, bounded semantic questions, decision intent,
validated outcome, applied view and frozen SDK generation plan. Intent ACK
precedes decision dispatch; outcome ACK precedes view application; generation
retains the SDK's authentication, request checks, provider capacity, private
continuation, stream settlement and cost reporting. Partial streams are display
observations and cannot authorize tools or fallback.

The compiler keeps mandatory instructions and tool exchanges intact. Optional
history can be full, an exact UTF-8 extract, a generated historical summary or
omitted from one view. Recall restores original groups. Summaries and extracts
retain their immutable source commitments; known workspace version changes
invalidate their reuse. With unknown versions they remain explicitly historical
and never certify current workspace contents.

Workers receive admitted references to shared evidence, construct independent
views and return conclusion references. Foreign tool exchanges render as
historical data so their call IDs cannot enter another worker's live protocol.
Fresh and independent-review workers do not inherit parent inventories. Native
large tool bodies are committed as checksum-bound artifacts before their result;
task-scoped reads recover complete bodies across hot and cold continuation.

When model policy is explicitly enabled, a finite configured set of at most
16 generation models participates in the same decision request. Independent Noul
questions judge task suitability; code compares eligible models against the
routed and full context views, enforcing declared byte limits, explicit cost
assumptions and switch hysteresis. A fixed model and manual effort remain hard
constraints. The chosen model/view binding, alternatives and assumptions are
persisted before SDK dispatch. Failed or uncertain judgments preserve the
configured fallback. SDK model policy is not applied a second time to this
binding.

Planning estimates use four bytes per token, declared token prices, output
reservation, optional prefix-loss penalty and a model-switch penalty. Exact
prompt-prefix overlap is observable; cache availability and discounted billing
are not inferred from it. These estimates are separate from actual generation
usage, decision usage and reported/reconciled charges. No quality, cost savings
or cache-hit gains are claimed from fixture tests.

Native consecutive Turns retain the same Core session. Quiescent nonterminal
checkpoints can resume the original Turn only after the Thread owner is known
stopped and all effects/accounting are reconciled. Running or uncertain work
cannot be automatically reissued. Steering uses the original receipt identity,
quiesces stale output and applies once at a safe context boundary. Fresh startup
instructions enter the restored context without replaying completed tools.

Public Thread events and the native TUI expose task state, selected model, view
size, omitted groups, result references and separate decision token estimates.
`bro code --model-policy` and Create-Thread `model_mode: policy` opt into model
selection; existing callers retain fixed models. See
`skills/bitrouter/references/decision-native.md` for configuration and bounds.

## Persistence and bounds

Native online checkpoints have a 2 MiB bound. The local artifact journal retains
up to 32 MiB of bodies; this implementation does not claim unlimited history or
lazy loading of every evidence block. Capacity exhaustion stops admission without
deleting evidence. Native runtime format 6 accepts formats 2 through 5 and frames
large Core checkpoint records across bounded recovery pages in one transaction.
Readers reject incomplete, reordered or corrupt pieces before interpreting Core
state. Native Item projections are independently checked against Core facts.

Large tool-output offload preserves the complete admitted result. Source tool
caps still apply (for example shell output limits). Unknown cost, unknown effects,
failed artifact commits, stale decision sources and lost durable ACKs retain
explicit uncertainty and cannot authorize another attempt.

## Validation status

The workspace run plus the corrected persisted-format assertion's focused rerun
cover 4,263 passing tests and 23 configured ignored tests. This includes 178
Agent/Thread unit tests and 309 Core integration tests. Local HTTP/replay fixtures
cover fixed-model and effort constraints, model capacity, low confidence,
decision service failure, switch hysteresis, private continuation, multi-page
checkpoint recovery and incomplete prefix rejection. Historical summary/extract
tests cover both known and unknown workspace versions.

Strict workspace clippy with all targets/features, formatting and distribution
checks pass. Configuration schema and registry output are current. No benchmark
experiments or live paid-model calls are part of this implementation.
