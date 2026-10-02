# ACP checkpoint evolution

Status: implemented with controlled serving acceptance and a user-authorized
real-task reward pilot. Natural-history calibration requires a separate corpus;
no live routing benefit has been measured. See the
[validation and experiment records](ACP_EVOLUTION_EXPERIMENTS.md).

The branch currently has rubric preparation/submission, leased judge jobs,
owner-scoped durable admission, closed enrollment cohorts, and effective-label
reconstruction with publication fences. The assembled model pipeline now includes
canonical admission, complete-route selection before named policies, routing
reload fences, execution/settlement recording, and an acknowledged gateway
inventory feeding checkpoint resource observations. A daemon-owned worker now
discovers stopped prefixes, resumes automatic judge jobs, refreshes resources
and reconciles eligible blocks, including separate quality monitoring after
adoption. Local operator actions are available through
the control socket and CLI. The local coding TUI now exposes mode/judge
selection, manual rubric review, typed candidate creation and block reconciliation.
The maintained Codex and Claude worker terminal campaigns now cover real tool
execution, automatic checkpoint scoring, default-parameter adoption, monitored
quality rollback and subsequent baseline dispatch. Their upstream replies and
rubric labels are controlled fixtures. Historical judge assessment remains
incomplete; passing fixture tests must not be reported as a live routing benefit.

The later [real subscription pilot](ACP_SUBSCRIPTION_PILOT.md) adds actual
model-generated controlled coding evidence and sealed-reference comparisons.
Its rubric responsibility/applicability and boundary-projection findings led to
the v2 contract and a [four-family fresh-task comparison](ACP_RUBRIC_V2_HOLDOUT.md).
That comparison verifies the revised behavior on those tasks, including unknown
validation for a fixed-environment blocker and unchanged completed legacy-job
retrieval. Both studies are distinct from natural historical usage and do not
establish live routing benefit.

## Scope and acceptance

The feedback source is the existing, app-owned ACP canonical store. Evaluating a
checkpoint must not execute code, access a repository, query CI, or create agent
rollouts. A configurable language model may judge the recorded evidence; manual
evaluation must work without any model call. Neither mode creates a task-specific
goal before execution. Evolution is disabled by default.

The implementation and accompanying experiments must establish three separate
claims:

1. Evidence-grounded rubric assessment: independent reference assessments on
   recorded actual coding work, compared with dialogue heuristics and a scalar
   judge. The user subsequently authorized constructed real-model tasks as the
   first pilot's data source. References must be sealed before comparator results
   and identified as model-generated. Controlled tasks must not be reported as
   naturally sampled history; fixed upstream fixtures must not substitute for
   model-generated coding evidence or human labels.
2. Controlled learner behavior: delayed, missing, revised and correlated feedback;
   cold start; bounded exposure; rollback; and observed learning curves across
   repeated seeded simulations. Simulation reward is not a live routing result.
3. An integrated serving implementation: capture, checkpoint, assessment, sticky
   policy-block assignment, actual execution records, effective-observation
   reconstruction, batch Thompson sampling, promotion, rollback and mode control.
   Real users need not be enrolled to test the serving implementation.

## Units and invariants

- A checkpoint freezes an immutable native-session prefix. Later content and
  corrected assessments replace its contribution to learning, not its history.
- An arm is a complete block policy, including its fallback. One native session
  keeps its assignment within an experiment across reconnects and new checkpoints.
- A model request is a diagnostic and metering unit, not a terminal reward sample.
- Fork families are correlated. Their labels cannot inflate the learner's
  independent sample count. Simultaneous blocks retain their joint assignment;
  interacting rules must be treated as a joint candidate or explicitly modeled.
- The comparison follows assignment intent, retaining fallback, failed and
  cancelled executions. Missing observations remain missing.
- Context contains only information visible before assignment. Post-hoc rubric
  branches and outcome diagnostics cannot be used as hindsight covariates.
- Cost is a union of unique recorded model request IDs over the whole session;
  unknown cost is not zero. Judge cost is reported separately and included in the
  feature's net cost. Evaluation has no product-level token or cost ceiling.
- Selection, rubric library, aggregation, judge, arm and learner versions are
  persisted. A version change cannot silently combine incomparable observations.
- Mode changes fence outstanding work. Turning evolution off stops new judging,
  exploratory assignment and promotion, restores unpromoted trials at supported
  boundaries, and retains adopted baseline changes.

An explicit operator `restore` withdraws the current experiment even while Off.
It requires the reviewed experiment identity, routing revision and a nonempty
reason. The owner control transaction records `operator_restore` separately
from measured reward evidence, advances the block revision and leaves its
assignments and observations intact. Unrelated blocks and mode settings remain
unchanged. Identical retries return the original receipt, including after a
newer experiment exists; other stale targets are rejected. Serving resolves
the last supported baseline with live dependency checks, using configured
routing when those checks fail. Session overrides keep precedence and already
dispatched calls are not restarted. The CLI and TUI share this operation. The
TUI reviews the target and reason before submission and exposes the experiment's
  publication timeline, including automatic and operator withdrawals.

Checkpoint review also returns its own immutable assessment revision history,
including stored evaluator identity, selection outcome and retractions. The
draft's expected revision still fences the session's selected assessment; its
prefill revision identifies only the stored starting point. These identities
are distinct when correcting a historical prefix. Current selection wins;
for an older checkpoint, the latest manual revision takes precedence over
automatic annotations. A retraction does not resurrect an earlier score, and
obsolete rubric formats remain inspectable without conversion. Reading history
does not publish or alter a draft. Opening another native session starts a new
eligibility unit while retaining the same agent's launch routing/model/timeout;
it does not reassign or replay the previous session.

## Rubric contract

A fixed, versioned library defines scoring anchors and weights. After execution,
the selector supplies applicability, rationale and original evidence citations
for every library item. Required obligations cannot disappear because no test,
review or PR event was recorded. Missing evidence is unknown, not non-applicability
or success. All score and diagnostic citations must resolve within the frozen
prefix. Tool execution status is evidence about execution, not proof that the
overall task was solved. A test result does not validate later modifications.

For applicable weights W, known scores r and unknown weights U, the aggregation
reports sum(w*r)/W and (sum(w*r)+U)/W. These are missing-value bounds, not statistical
confidence bounds. Severe violations and required unknowns remain separate gates.

The real-task pilot led to `coding-checkpoint-rubric-v2` and
`recorded-evidence-judge-v2`. Review resolution is conditional on this actor's
repair/acceptance obligation. Forbidden or explicitly deferred execution is not
an obligation at that checkpoint; permitted static inspection is delivery evidence.
A required check blocked by the environment remains applicable and unknown rather
than a code-quality failure. Current learner gates continue to reject incomplete
quality observations. The selector/judge follows explicit obligation, permission,
attempt, result and artifact-coverage decisions; explanations preserve these
facts, while automated validation checks contracts/citations rather than proving
the semantic judgment correct.

Quality evidence is versioned as `recorded-acp-quality-evidence-v2`. Session
boundaries project stop reasons or error codes/messages only; nested usage/model
metadata stays in the original canonical record and resource surfaces. Legitimate
task data is not recursively scrubbed. Version changes alter the measurement
contract, prevent old labels entering current aggregation, and retire incomplete
old-input jobs before another model attempt. Historical records are not rewritten.

## Learner contract

The first learner uses batched posterior sampling over baseline/challenger arms.
Continuous rubric observations require an explicit continuous likelihood; they
are not fractional Bernoulli trials. The implementation must identify its prior
and likelihood assumptions and must not label model-based posterior probabilities
as distribution-free guarantees.

The local TUI can create a named block with multiple related preset/virtual-route
changes and default TS parameters. Its preview binds source, owner, evaluator
contract, complete routes, control generation and mode epoch. Registration
rechecks the preview under the owner control transaction and live route fences;
an unchanged retry acknowledges the existing experiment without resetting it or
changing mode. The current session is not reassigned. Manual and configured-judge
feedback use exactly the same versioned measurement contracts as their scoring
paths. Separate block independence remains an explicit user assumption, not a
consequence of having different matchers. The CLI retains advanced fingerprint,
dependency and parameter definitions. Both surfaces support sequential revisions
of an existing block, as described below.

Batch sampling produces an explicit categorical assignment probability. The
deployed probability after baseline retention and exposure limits is logged.
Pending observations cannot authorize expansion. A compatible candidate with a
versioned prior can receive a first trial without prior candidate observations.
Learner v2 retains the last positive allocation, capped at the configured initial
exposure, while either arm lacks the minimum independent quality, cost or latency
families. This bounded warm-up prevents a resource prior on a different scale
from starving an unmeasured arm after the other arm's first result. Pending,
cumulative exposure, severe-violation and recent-quality guards still apply.
Once both arms have sufficient evidence, allocation follows the posterior joint
benefit probability subject to the existing limits. This warm-up may expose more
sessions to a poor candidate before a quality guard fires; it is not a guarantee
against incorrect feedback. At the default 10% initial rate, twenty challenger
families require roughly two hundred eligible independent sessions on average,
and missing feedback can extend that period.

Learner v3 retains these limits and uses common numerical posterior draws per
experiment and learner version. Revising assessment provenance or closing a
capture must not reroll an unchanged posterior across a decision threshold.
Effective numerical corrections still update the posterior. Full source
revisions remain in evidence digests, plan identities and publication fences;
an outdated snapshot remains unpublishable even when its numeric values match.
The production controller experiment uses the same seed derivation. These
common integration draws are separate from random native-session assignments;
they reduce numerical instability and do not establish statistical validity of
the reward model or a time-uniform guarantee for repeated decisions.

An incompatible persisted learner plan cannot admit new trial sessions until
reconciliation. Equal evidence is deduplicated only when learner, configuration
and measurement contracts also match. Revalidation does not rescore the rubric,
reassign existing native sessions, restore a reduced positive exposure rate or
restart a withdrawn experiment. In particular, a legacy experiment already at
2% remains at that rate; a fresh explicitly registered experiment uses its own
configured initial allocation.
If the new plan recommends promotion before its cohort has closed and resolved,
reconciliation stores the validated plan without adopting it. This lets the
cohort finish after an upgrade. Readiness can then enact that same plan once;
repeated polling before or after readiness cannot repeat publication.
Promotion separately requires comparable observations, quality noninferiority,
an absolute quality floor, and resource evidence. Local diagnostic blame is not
causal credit.

Learner state is reconstructed from effective observations so retraction,
deletion, source gaps and assessment revisions remove stale contributions.
State-changing operations use durable compare-and-set semantics. Publication and
rollback change the target block against the latest combination, preserving
unrelated blocks. Restart, concurrency and interrupted jobs are acceptance cases.

Admission commits the joint native-session assignment and request intent before
dispatch. Concurrent admissions serialize through the owner's control record.
Recognized ACP command-list, configuration, mode and session-info notifications
do not mark a new session as already executed. They remain in canonical evidence.
Assistant content, tools, usage, unknown/malformed notifications and completed
prompt turns still prevent first-time trial enrollment. This includes worker
startup warnings emitted as ordinary assistant content: the gateway cannot
reliably distinguish them from model output by text matching. A session rejected
at its first request does not enter the experiment on a subsequent request.
A cohort closes when its configured size or outstanding candidate limit is
reached; reconnects do not enter a new cohort. Only resolved cohort feedback can
change the next cohort's allocation. Evidence-seeded planning and evidence-digest
idempotency prevent repeated polling from manufacturing another exploration
update. A quality withdrawal terminates the experiment and cannot be undone by
another Monte Carlo draw.

A completed assessment with unknown quality is resolved for cohort bookkeeping,
but remains incomplete for learning. A zero allocation holds new trial admission
even after that cohort closes; the last positive exploration rate is remembered
only for a later evidence-supported resume. Missing or unpriced observations
cannot be turned into additional candidate exposure by reopening a cohort.
Admission carries incomplete candidate observations across cohort boundaries and
adds reservations made since the last validated plan to that outstanding count.
The pending limit is not a fresh allowance for each new cohort.

Feedback mode and trial epochs are separate. Manual/automatic changes fence
judge jobs while preserving the session's trial assignment. Off/on revokes the
old unpromoted trial permanently for that native session; adopted blocks remain
the baseline. A judge-model change advances the feedback epoch even if the mode
stays automatic. Judge dispatch and assessment selection enforce the feedback
epoch; the serving route hook fences control changes before execution. The
checkpoint scheduler checks that same epoch inside checkpoint creation and
cursor persistence, and automatic job enqueue checks it before committing work.

The learner uses a normal-inverse-gamma working model with unknown observation
variance and samples the mean from its marginal Student-t posterior. The prior
variance parameter is its initial expectation, not fixed known noise. Initial
fixed-variance experiments never recommended promotion even in the low-noise
beneficial case; that failure motivated learning dispersion as well as the mean.
A regression case requires a good candidate to graduate within the default
trial quota. This does not establish calibration on actual coding outcomes.

A recent-family quality guard detects degradation hidden by a large successful
history, but is not a nonstationary-bandit regret guarantee. Mean/noise priors,
minimum evidence, quality constraints and resource gates remain explicit. A
minimum sample count alone is insufficient to demonstrate useful learning.

### Monitoring after adoption

While evolution is enabled, newly admitted sessions using an adopted policy get
a durable monitoring membership with its experiment, adoption revision, admission
sequence and reference blocks. Original trial assignments retain their arms.
Monitoring membership survives reconnects; a session first admitted while off is
not retroactively enrolled when evolution is enabled again.

The learner reconstructs these observations separately from randomized trial
evidence, under the same canonical, assessment, inventory and publication fences.
The most recently admitted families define the monitoring window before missing
feedback is excluded. A missing member makes that family's numeric quality
unknown. Forks and revisions do not increase independent sample counts or make
old evidence recent. A supported severe violation, or sufficient posterior
evidence that recent quality is below the absolute floor, withdraws the adopted
block. That withdrawal is latched and preserves unrelated blocks.

The original trial continues to determine whether its promotion is supported:
revisions can invalidate that support. Revalidation that still supports adoption
does not change the policy revision or invalidate dependent blocks. Monitoring
observations never increase the randomized trial count or provide additional
cost-benefit evidence. Without a concurrent randomized control, this absolute
quality alarm does not establish continuing noninferiority, comparative savings
or a regret guarantee. It also cannot detect convincing incorrect feedback before
contradicting evidence becomes available.

### Contextual extension and algorithm comparisons

The initial implementation keeps independent baseline/challenger posteriors
within each policy block. Blocks provide coarse context segmentation, but this
does not transfer evidence between blocks or estimate how a policy's outcome
changes with continuous session features.

A linear outcome model and an exploration rule are separate choices. A linear
model assumes the conditional mean has the form `E[R | x, a] = phi(x, a)^T theta`.
Linear Thompson sampling draws `theta` from a posterior and selects using the
sampled prediction; LinUCB uses an optimistic confidence score. LinUCB does not
require a Bayesian prior. Selecting Thompson sampling for the first version
therefore does not exclude a later linear contextual model.

Keep block Thompson sampling as the initial serving learner. Consider linear
Thompson sampling when recorded, pre-assignment features predict repeatable
differences in policy outcomes, block-level samples are sparse, and a shared
model improves held-out prediction and uncertainty calibration. Use LinUCB as an
experimental comparator with the same features, observations, candidate set and
quality/exposure gates. It is not a second serving controller or a prerequisite
for the initial feature. A disjoint per-arm regression alone does not transfer
evidence to unseen arms; such transfer requires explicit shared parameters and
validated action features. It must not bypass cold-start exposure limits.

Features must be frozen at the policy-block assignment boundary. Initial request
length, already-declared task properties and available tool capabilities can be
eligible; later test results, review findings, final rubric applicability and PR
outcomes are labels or diagnostics, not assignment-time features. Describing an
initial request does not create a task-specific scoring goal before execution.
Post-assignment observations from a continued session are also consequences of
the current policy and cannot silently be used to reassign a sticky experiment.

Compare block TS with linear TS to test contextual generalization, and compare
linear TS with LinUCB to isolate the exploration rule. Hold feedback processing,
family handling, costs, quality gates and publication semantics fixed. Report
sample efficiency, quality violations, total cost and cold-start performance;
prefer the simpler learner if the contextual model provides no measured benefit.
Neither linear modeling nor an optimism rule resolves delayed reward attribution,
incorrect judge labels, correlated forks, or block interactions. Classical linear
bandit guarantees do not automatically apply to this checkpoint feedback system.

See the original [LinUCB model](https://arxiv.org/abs/1003.0146) and
[linear Thompson sampling analysis](https://proceedings.mlr.press/v28/agrawal13.html).

## Automatic feedback and local control

The serving daemon owns a cancellable worker with a two-second polling interval.
It scans the existing canonical store; it does not run in the capture append or
coding response path. A matching `session/prompt` response, with no outstanding
recorded prompt, makes the current prefix eligible. This is a recorded stop
boundary, not a declaration that the user's entire task is finished. A native
session may continue and acquire a later checkpoint.

Each feedback epoch records its start time. Discovery considers stops recorded
after that time. Enabling automatic mode or changing the judge does not silently
bulk-evaluate earlier stopped sessions. Explicit checkpoint commands remain the
way to evaluate historical prefixes. Manual mode discovers checkpoints without
calling a model. Off mode creates no automatic checkpoint, model attempt or
learning publication. It may repair the metadata of an assessment that already
committed before shutdown or the mode change.

Per-session scan cursors, immutable checkpoints and leased jobs survive daemon
restart. Each automatic job carries its checkpoint, judge version/model,
feedback epoch and expected assessment revision. An appended prefix, corrected
assessment or changed epoch supersedes queued work. In-flight responses can be
cached, but stale jobs cannot replace the current assessment. Job-receipt recovery
validates the cached input, output and committed canonical revision before
marking an interrupted job complete. Shutdown cancels the worker and preserves
the request identity and uncertain lease of an interrupted attempt.

The worker processes eligible jobs serially; a slow model can delay other
background feedback. Its polling interval is not a completion deadline. Jobs
allow at most three model attempts and expose exhausted retries as unfinished
work. This retry policy is not a token or cost budget. A cached response can
still be submitted without another model attempt. Renewing an exhausted
evaluation as a new explicit job is not yet supported.

`bro acp evolution --config PATH` addresses the existing local serving
daemon. `status` reports mode, blocks, worker progress, checkpoint cursors and
job summaries. `mode off|manual|automatic` changes feedback mode; automatic
requires a saved or supplied `--judge-model`. `register FILE` validates a complete
block definition against live routes without enabling evolution. `learning BLOCK`
inspects evidence and a proposed plan, while `improve BLOCK` reconciles and may
publish an eligible block change. The background worker uses the same live-route
validation and canonical publication fences.

`LearningReport.minimum_families_per_arm` is an additive optional field containing
the reviewed experiment's configured minimum, including when inspecting an
archived experiment. It is not taken from the current version of the block when
the requested experiment is older. Older daemon responses may omit this field;
absence means unavailable, not zero or an inferred product default. The TUI
shows baseline and candidate usable session-group counts separately from each
arm's quality, log-cost and log-latency `observed_families`. Assigned sessions,
repeated checkpoint assessments and prior strength do not add observed groups;
related forks share a group. These counts describe usable evidence for each
measure, not proof of statistical independence. Reaching the configured minimum
does not itself satisfy the quality and resource gates for adoption. This
read-only presentation does not change the learner or admission policy.

The local coding TUI's `/evolution` flow uses that control socket through the
application action port. The conversation driver retains only a transient
review/candidate draft and presentation data, with no direct database or daemon access.
Manual review can freeze the observed prefix or reopen an existing checkpoint,
prefill a prior structured rubric, select original evidence, explain scores,
mark supported violations and preview the aggregate before submission. The
draft retains its checkpoint, expected revision and idempotent submission ID.
Each edit advances the submission identity; a stale draft cannot overwrite a
newer canonical assessment. Opening another session clears the local draft.
Unknowns and non-applicability remain distinct and positive verification still
requires recorded tool evidence. Model selection atomically preserves the
current feedback mode, including an Off set by another local client.

TUI controls are absent from remote/explicit-socket operations-only surfaces.
They operate on the local owner and do not create agent rollouts, execute tests
or call a judge during manual review or candidate preparation. Candidate creation
selects complete routes, supports several related rules, refreshes the configured
evaluator catalog and previews actual routing dependencies before registration.
The transaction checks source definition, owner, mode/control revisions and live
routes. Repeated identical registration acknowledges the same experiment, even
when the original response was lost and the mode changed afterward. The draft
survives a coding-transport disconnect but is cleared when another session opens.
Candidate and manual review drafts are mutually exclusive.

Policy priors contain floating-point values. Exact JSON float round trips are
enabled so IPC and persistence preserve their versioned digests; otherwise even
an unchanged reviewed definition can differ by one bit after deserialization.
Assessment-revision history is available through the terminal, including the
stored revision used to prefill a correction and the current revision that
fences its submission. Broader capture-to-publication acceptance is tracked in
the experiment records; fixture success does not establish evaluator accuracy.

### Sequential experiment revisions

A stable policy-block ID has one current experiment and a retained archive of
its predecessors. Revising preserves the source and exact selector/fingerprint
matcher set, so an iteration cannot silently discard part of an adopted policy.
Related candidate routes can change together. Baselines inherit the last
supported routes; the runtime requires a reset to configured selectors when the
previous live routing contract or declared dependency changed. A new experiment
starts fresh priors, cohorts and exposure counters. Prior samples remain in
their original learner. Revision registration preserves the feedback mode.

Each enrollment pins the current experiment identities, including an explicit
empty set when no blocks existed. Legacy records use their assignment/adoption
identity, or the original experiment baseline. A revision never moves an
existing session into its new arm or opportunistically admits a nonparticipant.
Retired versions serve their existing sessions under the same Off and rollback
rules. Admission validates live contracts for both current and archived versions
and rejects a concurrent control-generation change while resolving those
contracts. Request compatibility and explicit override precedence still apply.

Archived trials cannot enroll new sessions or gain a new adoption. Their
existing assignments and monitoring memberships still receive checkpoint
corrections. Reconciliation can withdraw an archived adoption; every subsequent
version whose baseline inherited that adoption is then withdrawn as well.
Routing resolves the first unsupported ancestor's baseline, retaining earlier
supported adoptions. Unrelated blocks remain unchanged; declared policy-revision
dependencies still invalidate when a deployed baseline changes. New descendants'
outcomes are not copied into the archived trial or its monitoring population.

Starting another trial without changing its supported baseline preserves the
policy dependency revision. A separate experiment ID and predecessor chain
identify the new learner. Publication history and registration receipts remain
available, including retries after subsequent revisions. CLI `--experiment` and
the TUI history inspector select a specific version; reconciliation from a TUI
evidence view is pinned to that version. The revision preview retains TS settings
and declared dependencies, and reports baseline inheritance or required rebase.
Changing the matcher partition is outside this revision operation.

## Serving boundary

The assembled pre-request order is authentication, session normalization,
provider continuation, canonical evolution, and access policy. Evolution requires
an active canonical connection matching the authenticated credential's route
scope and declared controller. It resolves a recorded ACP session, or a recorded
native thread/root when the ACP header is absent. Unrecorded or ambiguous sources
do not authorize a route change. Controller headers are correlation claims, not
authentication. Every request under an active recorded controller enters the
gateway inventory before dispatch, including unresolved or ambiguous session
identities. Its admission remains visible when later routing preparation fails.

Before forwarding capture, the local controller registers its connection with
the serving daemon over the local control socket. This verifies the database,
principal/controller namespace and inventory runtime epoch. A legacy daemon or
an unavailable control socket leaves content recording usable but without cost
coverage. Registration after recorded events or gateway requests is rejected.
Missing coverage prevents new experiment enrollment while preserving adopted
baselines. A serving restart during capture invalidates the acknowledgement when
the new runtime observes traffic; reconnecting establishes a fresh boundary.

Operators register blocks against the live config and named-policy snapshots.
Matchers name configured presets or virtual models. Registration does not enable
evolution. Selected arms retain preset defaults, policy tool guards, effort
validation and fallback semantics because evolution changes the complete selector
before named model selection. Session route overrides and provider continuations
retain precedence, while their sessions keep assignment intent. A candidate that
lacks positive evidence for the request's capabilities uses the established route and records
both intended and actual selection. The compatibility check conservatively checks
all possible named-policy tiers; this may retain the baseline even when one tier
would have sufficed.

Per-block dependency digests include relevant policies/certificates, complete
route chains across inbound protocols, nonsecret target compatibility and pricing,
and timeouts/backoff. An unrelated route does not invalidate the block. URL
credentials, query strings and fragments are not admitted into this dependency
contract; those endpoints remain available to ordinary routing. API credentials
are neither included in the digest input nor stored in execution evidence.
Process-local config generations and policy snapshot identity detect reloads
between admission and route resolution, including A-to-B-to-A config changes.
This is not a transaction across every reload participant; mixed executor-timeout
and policy-table reload states still require end-to-end validation.

A durable execution starts before dispatch. It retains one request identity,
canonical connection/watermark, intended arm, effective route, actual upstream
attempts and terminal settlement. Replaying that identity cannot make another
model call or replace its original metering/eval evidence. Interrupted executions
remain unresolved. Metering's final cost is recorded separately from complete
request cost; estimated usage and multiple attempts without complete attempt-level
charge evidence remain unknown. A terminal observer distinguishes clean completion,
failure and client disconnect even when cancellation has no error code. Replay
rejection also applies after the original capture connection has closed.

Resource completeness is explicitly scoped to **BitRouter-managed model requests**;
the inventory covers the registered authenticated principal/controller namespace.
Calls bypassing the gateway or omitting that controller identity are outside this
contract. The handshake acknowledges serving support; it does not itself prove
that a harness propagates the identity on every model call. That propagation
remains part of the full capture-to-publication acceptance test. A complete observation
requires acknowledged healthy capture, closed prompt boundaries, no canonical
gaps, no unresolved request membership, and a settled terminal outcome with
complete attempt cost for each unique request. Full inherited prefixes are joined
without duplicating requests; later parent work is excluded from a fork prefix.
Resource membership `native-head-resources-v2` includes this native session's
gateway admissions at or before its checkpoint head, including auxiliary calls
after a prompt stops at the same head. The next prompt advances the canonical
head before dispatch. Inherited parent segments keep their timestamp cutoff, so
later parent calls remain excluded from a fork. Unresolved requests at a stopped
head remain unknown through the next event in that native session, including
when other sessions advance their shared connection. Missing settlement keeps
the cost incomplete. Older timestamp-bound resource records remain readable but
must be refreshed before contributing cost to the learner.
Completeness is as of the observed inventory. Final worker-exit accounting also
requires clean capture closure and settled outcomes for every observed request;
an open-session snapshot cannot promise that no further work will occur.
Late settlement creates a new resource revision without changing content or
adding an independent reward sample. Known subtotals remain inspectable when
coverage, prices or attempts are incomplete.

A successful fork response belongs to the child session. A parent prefix can
reference that response to close its own fork request only when the response
precedes an included event on the same capture connection. The referenced child
record is a deletion dependency, not an inheritance of the child's subsequent
work. A prefix ending at the fork request remains incomplete until it advances.

Every inventory admission and settlement advances its source connections'
coverage revisions. Resource persistence and learning publication recheck these
revisions under the same connection locks as capture. A newly unresolved request
or a runtime change can therefore invalidate prepared cost evidence even when
the canonical session head did not change. Correlation-only historical metering
cannot establish complete coverage.

### Judge overhead accounting

Each new judge request reserves a durable cost record in the same transaction
as its job attempt. The assembled pipeline recognizes the reserved request ID
without adding ACP identity or inheriting coding experiment membership. It
records dispatch, attempted provider hops, settlement and terminal outcome.
Reusing an already dispatched ID is rejected without overwriting its metering.
The record contains identity and execution metadata, not judge input or output.

Owner-local status joins retained request reservations, current jobs and current
metering evidence. It exposes owner, native-session and job totals, a retry
subset, and per-request incompleteness reasons. Each request contributes once;
checkpoint revisions, forks and multiple policy blocks do not duplicate fees.
Failed, superseded and historical-checkpoint jobs remain in the spending view.
Deleting source content still removes cached judge text and blocks further
assessment submission. Content-free attempt metadata and an indexed owner
binding survive that deletion, so already incurred fees remain visible even
when the original job details have been removed. This retained subset is
identified separately and is already included in the owner total.
A cached response or completed-job retry does not reserve another request.

Complete cost evidence requires a terminal tracked attempt, observed usage and
charge evidence, and completed reconciliation when required. A terminal failure
with no upstream attempt proves zero model spend. The cost record preserves the
pipeline's original usage provenance before metering's legacy normalization.
Inferred zero usage for rate-limit/policy rejections cannot establish observed
zero cost. A later authoritative receipt may resolve the missing evidence.
Missing inventory, unknown usage/prices, interrupted attempts and missing earlier fallback-hop costs leave
the total unknown. The current final-request meter cannot prove all fallback
charges, even when its final hop succeeded. Legacy jobs without an execution
inventory can expose metered subtotals but cannot acquire complete coverage
retroactively. Deleted legacy jobs that never reserved cost metadata cannot be
reconstructed from this ledger. Stored estimates refresh from late receipts
without reevaluation; they are not invoices or statistical lower bounds.

Judge overhead is separate from coding outcomes and is not allocated to every
block. This spending report does not estimate counterfactual coding cost or net
routing savings. Including the overhead once in a validated whole-feature
quality/cost comparison remains part of the net-cost acceptance experiment.

## Validation protocol

The research evaluator's reference labels are sealed before exposing baseline
outputs. All methods score the same frozen input. Reports include provenance,
coverage, false success, missed failure, disagreements and adjudications. The
user-authorized real-task pilot is completed separately from the prepared
natural-history protocol. Controlled upstream fixtures only validate mechanics;
the pilot uses actual generated coding work and recorded tool observations.

Simulation compares fixed allocation and Thompson sampling under the same
generated potential outcomes and constraints. It reports cumulative reward,
cost, candidate exposure, pending feedback, effective sample counts and incorrect
promotion. Actual serving tests use isolated configurations and deterministic
local upstreams; they do not alter the operator's current routing configuration.

References: [Thompson sampling](https://proceedings.mlr.press/v28/agrawal13.html),
[delayed feedback](https://proceedings.mlr.press/v28/joulani13.html),
[conservative contextual bandits](https://arxiv.org/abs/1611.06426).
