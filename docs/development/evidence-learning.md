# Recording, checkpoints and learning

This guide describes the current implementation and operator-visible boundaries
for ACP evidence. Update it with changes to storage, evaluation or scheduling.
The [CLI reference](../CLI.md#local-acp-recordings) documents commands; exchange wire
formats are maintained in the [service API](../specs/service-api/README.md).

## Observed recording

Recording is opt-in and app-owned. It persists only content visible through the
selected ACP connection; it does not inspect private harness logs, repositories
or task outcomes. Source/session identities and observed event order remain
explicit. A reconnect cannot be treated as continuous observation when evidence
is missing. Content recording and metering are separate operations; record-write
failure must not invent a complete transcript or alter authoritative usage.
Deletion fences further recording for that identity and does not erase the
harness's own history or metering. Keep prompts and output out of operational
metadata unless the user explicitly enabled content capture.

## Immutable checkpoints and assessment revisions

A checkpoint freezes an observed content prefix and source lineage. Forks use
the parent watermark captured at the fork, not later parent work. Missing or
cross-scope lineage cannot be filled from ambient current state. Assessment
revisions do not rewrite checkpoint content, and resource observations are
versioned separately from content and quality. A late usage settlement does not
justify a new quality judgment or replace an old evidence timestamp.

Costs union request identities across inherited and own prefixes instead of
adding overlapping checkpoint totals. Unknown pricing, pending settlement,
ambiguous correlation and conflicting observations remain incomplete. Related
forks are a family, not independent task samples. Historical timestamps and
current configuration are not substitutes for retained route evidence.

## Evaluation and policy publication

Operational completion and transport success do not prove task success. Eval
stores externally attributed subjects, results and snapshots. Evaluation,
candidate compilation and serving publication are separate actions; recording
or submitting an assessment cannot silently change the active policy.

Evolution uses explicit mode, rubric and budget selection, immutable checkpoint
inputs and identified evaluation jobs. Reward, experiment assignment and policy
certificates retain their cohort and evidence provenance. Repeated assessment
revisions or correlated forks do not create independent trials. Replacing a
rubric/model or updating a policy does not retroactively rescore old evidence or
reassign already accepted sessions. Unknown effects and unconfirmed costs must
not be converted into positive reward. Policy file formats and guarded
publication are defined in the [configuration contract](../specs/configuration/README.md#policy-lock-format-4).

The current code is organized under `harness/acp`, `acp_trajectory`,
`storage/checkpoint` and `core/evolution` in the application. Keep dated campaigns,
raw outputs and acceptance receipts with their PR/CI artifacts; this guide
records the rules needed to use and maintain those components.
