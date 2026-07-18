# Finalization Exploration Guardrail Design

## Context

The policy-only Terminal-Bench short13 lineage
`terminus2-terra-c0c1-short13-policy-20260717T204852Z` completed r1 and r2,
but r3 case 11 (`path-tracing-reverse`) became runtime-invalid. The agent spent
exactly 45 minutes and ended with `AgentTimeoutError`. Its last three
finalization requests were:

1. an exploration request to the cheap tier that returned HTTP 504 after the
   120-second upstream timeout;
2. a second cheap-tier finalization request that returned HTTP 504 after the
   same timeout;
3. a capable-tier request selected after the reliability circuit opened that
   returned HTTP 200, but left only about 79 seconds before the agent deadline.

The same case succeeded in r1 and r2. The failure is therefore not evidence
that the case, harness, or capable model is intrinsically invalid. It exposes a
safety-boundary defect: finalization is a terminal, deadline-sensitive workflow
state, but the exploration gate currently treats it like an ordinary in-loop
state. Two otherwise bounded weak-route timeouts can consume the remaining task
budget before the reliability circuit has time to recover.

## Goals

- Prevent online exploration from downgrading finalization requests on every
  harness represented by the normalized Workflow State IR.
- Prevent a previously learned cheap lock from being applied during
  finalization.
- Preserve operator ownership: an explicit static finalization route may still
  select the cheap tier.
- Keep opening behavior, tool-use guardrails, adequacy pins, provider
  reliability circuits, and all non-finalization exploration behavior
  unchanged.
- Keep router and adequacy-observer eligibility decisions identical so a
  request that cannot be explored cannot advance exploration state.
- Run a fresh policy-only short13 lineage through r1, feedback, r2, feedback,
  and r3 with the immutable control artifact and fixed concurrency three.

## Non-goals

- Change the static policy table or rewrite an operator-authored finalization
  route.
- Add a new configuration switch. Finalization safety is a default invariant,
  like opening exploration being disabled by default, rather than an
  experiment-specific knob.
- Change the two-failure provider reliability threshold or upstream timeout.
- Add same-request provider fallback.
- Rerun control or reuse policy state from the rejected lineage.
- Hide or discard a failed benchmark attempt.

## Routing design

Add one shared exploration-eligibility rule in `PolicyTable`:

- `WorkflowStateKind::Finalization` is never eligible for online exploration.
- `WorkflowStateKind::Opening` retains the existing `explore_opening` rule.
- all other states retain the existing eligibility rule.

Both `PolicyTableRouter::exploration_allowed_for` and
`PolicyTable::exploration_allowed_for_prompt` consume the same state rule. The
first controls request-time selection; the second controls adequacy observation.
This prevents the two paths from drifting.

The guard applies only inside the exploration branch. Route selection still
starts from the static table, then applies the existing tool guardrail and
adequacy pin. Consequently:

- static `finalization -> capable` stays capable even if an exploration lock
  exists;
- static `finalization -> cheap` stays cheap because it is an explicit operator
  decision;
- a pinned static cheap route may still escalate through the existing adequacy
  safety path;
- explicit provider-qualified routes and BitRouter server-tool routes keep
  their existing opt-out behavior.

No new decision reason is needed. A protected finalization request records
`static_table`, `tool_guardrail`, or `adequacy_pin` according to the route it
actually takes; it never records `exploration_trial` or `exploration_locked`.

## Benchmark retry policy

The new lineage remains one accepted attempt per case by default. If a case is
runtime-invalid and the evidence proves an intermittent upstream transport
timeout, the operator may launch exactly one additional attempt for that case
before rejecting the round.

A retry is eligible only when all of these conditions hold:

1. the case has no valid TrialResult;
2. the terminal exception is an agent deadline or upstream transport timeout;
3. BitRouter request evidence contains a transient upstream status such as 429,
   502, 503, or 504, or an equivalent classified transport exception;
4. the central daemon, controller, and EC2 cleanup evidence is otherwise valid;
5. no previous retry exists for that case in the round.

Harness/session failures, verifier failures, model-quality failures, settlement
failures, configuration failures, and unclassified exceptions are not retry
eligible. The retry uses the same frozen BitRouter commit, policy database,
config, harness, model, task, AWS topology, and concurrency cap. It receives a
distinct attempt identity and an isolated Harbor result directory.

The first attempt is immutable and remains in the artifact. If a retry is
needed, acceptance must account for the cost of both attempts, record reward
zero for the failed attempt, use the retry TrialResult as the case outcome, and
preserve exact request/cost/reward/session joins. If the current driver cannot
prove these properties, it must fail closed and the retry mechanism must be
implemented and tested before launching attempt two. A second retry is never
allowed.

## Data and control flow

1. Decode the prompt into `OnlineWorkflowState` once per existing request path.
2. Resolve the static tier and existing hard guardrails.
3. Query the shared workflow-state exploration rule.
4. For finalization, skip both exploration trial and exploration-lock routing.
5. Apply the existing provider reliability permit to any selected non-capable
   route.
6. Record the final decision and serve the request.
7. At settlement, the observer recomputes the same eligibility rule and skips
   exploration learning for finalization.
8. Between accepted benchmark rounds, apply strict reward feedback exactly once
   and carry only that lineage's policy database forward.

## Error handling and invariants

- Unknown or malformed workflow state retains the existing conservative
  Terminus 2 handling; it is not treated as finalization.
- Static routing remains deterministic and operator-owned.
- Finalization protection does not depend on task reward, circuit state, or
  request cadence.
- No production `allow`, `unwrap`, `expect`, or `panic` is introduced.
- The immutable control artifact is referenced, never executed.
- Every AWS CLI call uses the explicit `benchmark-202607` IAM profile.
- Every accepted round must end with zero residual sandbox instances and dead
  round daemons.

## Verification

Rust tests must prove:

1. an eligible finalization request that would otherwise be an exploration
   trial remains on the capable static tier;
2. a finalization request with a learned cheap lock remains on the capable
   static tier;
3. an operator-authored static cheap finalization route remains cheap;
4. the adequacy observer skips finalization instead of counting a trial or
   cadence advance;
5. opening and ordinary in-loop exploration retain their existing behavior;
6. workflow-state extraction still identifies finalization across existing
   harness fixtures.

Repository verification includes the focused tests, full BitRouter tests,
workflow-state artifact tests, configuration tests, formatting, clippy with
warnings denied, and a Linux release build whose commit and SHA-256 are frozen
into the new run manifest.

Runtime acceptance requires r1, r2, and r3 to each produce 13 accepted case
outcomes, complete TrialResult evidence, all four settlement buckets, exact
cost/reward/session joins, authoritative settlement with no unknown rows, peak
concurrency no greater than three, and zero residual EC2 sandboxes. A retry, if
used, must additionally satisfy the attempt-aware evidence and cost rules above.
