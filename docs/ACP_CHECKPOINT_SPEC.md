# Immutable ACP checkpoints and assessment revisions

This application layer consumes the opt-in [canonical recording store](ACP_CANONICAL_CAPTURE_SPEC.md).
It provides durable inputs and revision selection for later evaluators. It does
not execute tests, inspect repositories, fetch PR state, invoke a judge, generate
task goals, or change routing configuration.

## Prefix contract

`acp checkpoints --agent SOURCE NATIVE_SESSION create --watermark N` requires
the observed current native-session head to equal N. Creation records source
event references and content digests, canonicalizer version, inherited fork
prefixes, gaps, and the preceding checkpoint reference. It does not duplicate
the transcript into another content store. Source identity is still the native
owner/source/session tuple; checkpoint hashes identify snapshots only.

The builder verifies source heads again under transaction locks before commit.
A concurrent append or deletion rejects a stale creation attempt. Repeating the
same freeze returns the same checkpoint. New appends create a new prefix, while
old reads resolve only the saved references. In particular, a later tool update
cannot change the status or output visible in an earlier checkpoint. Exports
verify each source digest and reject missing or changed source events.

Connection lifetime, source visibility, and label freshness are distinct. An
open connection permits a stable prefix. Interrupted capture, incomplete native
history, missing setup data, pending requests, and uncertain load/resume history
remain explicit gaps. These observations do not turn a partial prefix into a
quality pass or fail. A scoring layer decides which criteria it can evaluate.

Creation is an explicit service/CLI operation. Debounced automatic scheduling,
scoring workers, and TUI mode controls are separate consumers of this contract.

## Forks and families

A child checkpoint references its parent's watermark recorded at the fork
request, recursively through earlier forks. Parent work performed after that
boundary is excluded. The original setup request is retained by reference to
the native setup response, including the fork request when applicable. Missing
parent data is reported; cyclic or cross-scope lineage is rejected.

Every effective native-session result carries its family identity. The family
view retains separate native-session labels and reports one related cluster;
it never counts model calls, checkpoint retries, or assessment revisions as
independent session outcomes. Related forks are not asserted to be statistically
independent. A subsequent comparison layer must use that cluster information.

## Resource observations

Checkpoint creation also records an initial resource observation. Content and
resources are separate durable operations: if the resource write fails, the
valid checkpoint remains and the caller can retry. Refresh reads only existing
local metering and immutable route events. It does not fetch billing receipts.

A resource observation preserves its unique request set, actual model/provider,
available historical route/policy evidence, price state, and observation time.
Known cost uses a request-ID union across inherited and own prefixes. Unknown
pricing remains unknown. Reconciliation of a previously known request creates
a new observation without modifying the checkpoint or its assessment labels.
Consecutive identical observations are idempotent. If values change and later
return to an earlier value, that return is a new ordered resource revision;
it does not reactivate an old timestamp or leave a superseded value current.

Resource observations declare `membership_version`. The current
`native-head-resources-v2` rule includes authenticated gateway requests from this
native session whose admission observed a canonical head at or before the
checkpoint watermark. This includes auxiliary calls after prompt completion at
the same head. Their settlement advances the resource observation without
changing the content checkpoint or requiring another quality judgment. A new
prompt advances the content head before dispatch, so its calls cannot enter an
older checkpoint. Inherited parent segments retain the original timestamp
boundary; later parent work cannot enter a fork's resources.

Unresolved gateway requests remain unknown throughout this native session's
resource interval. Events from another session on the shared connection do not
silently exclude them. The next event in this native session bounds the interval;
subsequent unknown calls do not contaminate the older checkpoint. Pending calls
and missing prices keep cost incomplete. Every inventory change fences resource
selection and learning publication, including changes without a content append.

Historical observations without `membership_version` retain the earlier
timestamp-bound rule. Refresh appends a versioned resource revision; it does not
rewrite history or rescore content. The evolution learner requires the current
membership contract before using a complete cost. Completeness describes the
observed managed request inventory, not a promise that an open session will make
no further calls. Final closure additionally requires confirmed capture shutdown
and settlement of every observed request.

Legacy metering without authoritative canonical admission still uses temporal
correlation: its first local metering record or recorded route evidence must
precede the segment's event boundary. Late ambiguous requests remain unassigned.
Route events after that boundary are excluded, and current configuration never
fills historical gaps. The first metering timestamp is not a model start time.

Family totals union requests rather than adding session or checkpoint totals.
Conflicting resource snapshots for the same request mark its charge unknown
and expose the conflict. All totals explicitly retain incomplete metering;
unobserved local or remote requests cannot be assumed free.

## Assessment revisions and effective selection

The JSON submission envelope records evaluator identity/version, Human or
Agentic source, pipeline and selection digests, a criterion vector, evidence
references, and an explanation. Scores are `scored` with 0–1,000,000 ppm,
`unknown`, or `not_applicable`. The persistence layer validates range, identity,
and checkpoint references. Template applicability, semantic support, aggregation,
judge calibration, and promotion eligibility belong to the scoring layer;
importing a vector does not establish those properties.

Each submission supplies an idempotency key and explicit `expected_revision`
and `assessment` fields (null is allowed; omission is rejected). Reusing
the key for identical input returns the original result; different input is an
error. The expected revision is checked transactionally. A worker using an old
revision cannot overwrite a later correction.

Selection rules are explicit:

- A newly selected revision supersedes the prior native-session selection.
- An automatic result for the same checkpoint cannot replace a Human revision.
  It can remain in history with `human_revision_preserved` as its reason.
- A result for an older checkpoint remains historical and cannot replace a
  newer selected checkpoint, including when the older correction is manual.
- A new checkpoint can receive its own automatic assessment; an earlier Human
  score is not copied onto new content.
- An explicit Human retraction targets the current checkpoint and revision,
  records a reason, and leaves no effective assessment. Prior scores do not
  automatically revive.
- When the session appends, the previous label remains visible with
  `new_content_unassessed`. It is excluded from the current family label count
  until a new checkpoint receives a selected assessment. A failed import does
  not clear that freshness state or silently select an older label.
- If the session's own capture connection becomes interrupted after the
  checkpoint, the effective label becomes stale even if no new event could be
  persisted. Current capture states and the interruption reason are exposed;
  the immutable checkpoint and its original assessment remain unchanged.

The effective view returns one selection per native session, with its fixed
checkpoint, current source watermark, freshness, gaps and resource observation.
It detects concurrent source or selection changes while reading. Historical
revisions are audit records, not additional statistical samples. There is no
background optimizer or publication implied by a successful import.

## Deletion and recovery

Deleting recorded native content invalidates checkpoints that reference it,
including descendant checkpoints. Their resource observations and assessment
text are removed, and affected effective selections are cleared in the same
deletion transaction. Metadata tombstones fence in-flight writers. Checkpoint
references cannot resurrect deleted transcript content.

Checkpoint, revision, selection and resource records survive application restart.
They are stored in the configured application database with the same path
anchoring as capture, not in a UI timer or boot-local operation cache. No worker
or mode setting is required to inspect already stored history.

Large content columns use MySQL `LONGTEXT` and the other backends' `TEXT`.
The migration also widens MySQL's captured-event column without shrinking it
on downgrade. Backend message-size and storage failures still propagate;
this does not promise unlimited storage.

## Validation

Tests cover old tool versions, source integrity, stale watermarks, repeated
creation, revision retries and collisions, correction precedence, historical
submissions, retractions, late price reconciliation, ambiguous late request
membership, inherited prefix boundaries, family cost unions and deletion.
CLI validation also exercises persistence across separate processes. None of
these tests establishes judge accuracy or routing benefit.
