# Routing Evolution Evidence Design

## Objective

Turn one completed Harbor/Terminal-Bench run into conservative, auditable route
evidence while shifting the starter policy's progress guard from strong-only
protection to strong-or-balanced protection. The serving program remains
benchmark-neutral: it accepts only the existing Eval Exchange pass, fail, and
inconclusive contract and never learns Harbor or Terminal-Bench exception names.

This iteration does not publish an active policy, run a paid benchmark, or infer
an active economy route from missing data.

## Approved evidence boundary

The run contains 1,308 decisions. Static selection was 917 balanced, 294 strong,
and 97 economy, while the actual selection was 389 balanced, 879 strong, and 40
economy. Progress guard promotion affected 681 decisions. Replaying the same
decisions with `protected_tiers: [strong, balanced]` estimates 351 strong
(26.8%), 917 balanced (70.1%), and 40 economy (3.1%). These figures justify the
guard configuration change, but they are not route-quality evidence by
themselves.

Terminal task/verifier outcomes are quality evidence. Provider, network, auth,
rate-limit, transport, and equivalent infrastructure failures are reliability
events, not task failures. If a trial later recovers and obtains a verifier
reward, retain the terminal task result; excluded failed provider requests may
still contribute separately to reliability, cost, and latency.

## Chosen architecture

Use a configuration-first serving change plus an external deterministic
adapter:

1. Change the auto-router template's progress guard to protect both `strong` and
   `balanced`, keeping `strong` as its escalation tier.
2. Add a defensive invariant to the generic eval compiler: an
   `inconclusive` result cannot contribute quality counts, independent task
   identity, pass/fail weights, or quality-scoped hard violations even if a
   malformed or old adapter assigns positive quality credit. Explicit cost and
   latency credit remains aggregatable.
3. Extend `skills/evaluating-bitrouter-routes` with a Terminal-Bench/Harbor
   adapter, fixed exception taxonomy, evidence-matrix contract, fixtures, and
   executable tests. The adapter emits only generic Eval Exchange subjects and
   results at the program boundary.

This is preferred over two rejected alternatives. Adding benchmark exception
types to Rust would couple serving semantics to one evaluator and expand the
wire contract. Treating every request in a task as rewarded would inflate the
effective sample size and falsely credit unrelated route cells.

## External adapter inputs

The adapter accepts a Harbor run directory plus its BitRouter decision JSONL.
It reads only bounded, documented fields:

- trial identity, task identity, agent-execution timestamps, verifier reward,
  exception type/message, and request-latency observations from each Harbor
  `result.json`;
- router-authored decision identity, policy, policy digest, request key, route
  projection, selected/baseline tier, capture time, static tier, decision
  reason, progress-clause ids, latency, and cost from decision rows when
  present.

The adapter joins a decision to a trial only through an exact content identity:
an identical full message-prefix digest or a unique hashed task-description
field, followed by the exact router ingress digest. Execution-time proximity is
never a join key. Missing or ambiguous joins remain records, but cannot receive
quality credit. It never fabricates router decision ids. A run lacking
submit-ready decision ids can still produce the experience matrix and
diagnostics, but its packet list contains no invented attribution.

## Terminal outcome classification

The Terminal-Bench reference owns a versioned, ordered set of case-insensitive
exception rules. Each rule maps observed exception type/message markers to one
of:

- `provider`, `network`, `auth`, `rate_limit`, or `transport`: excluded from
  task-quality pass/fail evidence;
- `task`: an evaluator-confirmed task failure when a terminal verifier outcome
  exists;
- `unknown`: inconclusive, requiring operator review rather than defaulting to
  fail.

A terminal verifier reward takes precedence over an earlier recoverable request
failure. Reward `1` is pass and reward `0` is fail. Missing, non-finite, or
non-binary reward is inconclusive. An exception without a valid terminal reward
is inconclusive; if it matches a non-task rule, the adapter also increments the
excluded non-task-error count.

Every classification records its rule id. The taxonomy and rule ids live only
under the external skill.

## Unique quality attribution

A task or episode creates one quality observation, never one observation per
request. Positive quality credit is allowed only when all of these predicates
hold:

1. the terminal outcome is conclusive;
2. every decision is joined unambiguously and carries its router-authored id;
3. all quality-relevant decisions name one identical policy and route
   projection;
4. all quality-relevant decisions name one identical actual selected tier;
5. exactly one deterministic representative decision is chosen: the earliest
   `(captured_at, decision_id)` in that single cell/tier group.

Only that representative receives `quality.pass` credit with weight 1,000,000.
All other decisions may receive only separately observed cost or latency credit.
If any predicate fails, the adapter preserves the task packet with verdict
`inconclusive` and zero quality credit while recording the original terminal
outcome in redacted evidence attributes for audit. This keeps ambiguous outcomes
out of compiler quality evidence without losing the observation.

An economy observation that was followed by a strong recovery necessarily has
mixed selected tiers and therefore cannot satisfy unique attribution. It may
inform reliability and experiment design, but it cannot justify direct economy
adoption.

## Eval Exchange packets

The adapter emits one sealed subject and matching result per attributable task,
using the existing schema version 1:

- scope `task`, task identity as `subject_id`, evaluator kind `task_native`;
- copied router decision references only;
- redacted, content-addressed verifier and classification evidence;
- verdict pass/fail only for uniquely attributable conclusive evidence;
- verdict inconclusive with empty or zero-weight quality credit otherwise;
- explicit cost/latency credit only where those measurements belong to the
  representative router decision;
- stable evaluator configuration digest and idempotency key derived from the
  adapter version, taxonomy version, task id, and source artifact digest.

The script implements the same canonical evidence digest as the documented Eval
Exchange sealing contract and a test fixture validates its output with
BitRouter's real subject/result validators. Operators still submit packets and
own snapshot, candidate compile, diff, and publish decisions.

## Experience matrix

The adapter emits deterministic JSON and CSV matrices, sorted by
`(policy, route_projection)`. Every row includes at least:

- route projection, task family, role, and risk parsed from the canonical key;
- independent task count and episode count;
- pass, fail, and inconclusive counts;
- actual and static tier distributions;
- observed cost and latency aggregates;
- progress-guard promotion count;
- excluded non-task-error count with taxonomy rule ids;
- attribution-ambiguity count;
- critical-violation count;
- evidence grade;
- `quality_credit_eligible`, `active_recommendation`,
  `economy_experiment_candidate`, `controlled_validation_candidate`, and
  `screening_reason`.

Evidence grades are deterministic:

- `adoptable`: meets the direct economy gate below;
- `experiment`: stable low-risk balanced evidence suitable for an economy
  trial, but not direct adoption;
- `insufficient`: conclusive evidence exists but does not meet either gate;
- `inconclusive`: no uniquely attributable conclusive quality evidence.

Strict quality evidence and observational screening are separate layers. The
strict layer alone controls Eval Exchange quality credit, evidence grades, and
`active_recommendation`. The screening layer may associate a terminal passing
task with each exact-matched route cell it exercised, but it never emits quality
credit and is explicitly non-causal. A row may be a
`controlled_validation_candidate` when it has actual economy exposure or is a
normal-risk balanced cell, has at least five distinct passing-task
associations, has zero route-level non-task errors, zero recovery dependency,
and zero critical violations. `screening_reason` names the observational gate;
such a row requires a controlled future validation and is not publishable.

`economy_experiment_candidate` is true only for a normal-risk balanced row that
passes this screening gate. It does not change the row's active tier.

## Recommendation gates

An active economy recommendation requires every condition below for one route
cell:

1. economy was the actual selected tier for its quality observations;
2. at least five independent conclusive tasks;
3. pass rate at least 80% after excluding non-task errors;
4. zero critical violations;
5. no attributed task depends on a later strong recovery;
6. every credited outcome satisfies unique attribution;
7. zero excluded non-task-error contamination in the route cell.

No other route changes automatically. A balanced route may become an economy
experiment candidate only when it is normal risk, has at least five uniquely
attributable conclusive tasks, at least 80% pass rate, zero critical violations,
and no recovery dependency. It remains balanced in the shipped template. Strong
routes, guarded routes, ambiguous cells, and undersampled cells retain their
current tier.

The analysis artifact is advisory and never mutates `policy-lock.yaml`.
Economy routes derived from the separate analysis agent are integrated only
when its files exist and pass these gates; otherwise the implementation ships
no guessed active economy change.

## Template provenance

The progress-guard edit changes the policy value bound into the template
compiler digest. Refresh the compiler config digest by the repository's
canonical `auto_router_template_routes_have_deterministic_compiler_certificates`
test/helper path. Per-route evidence digests and the evidence root are based on
route identity and selected tier, so they change only if that canonical method
shows they must. Do not hand-author hashes.

Update the template prose to explain that balanced is protected for ordinary
progress continuity while strong remains the escalation tier. Existing runtime
defaults and user policy locks remain unchanged.

## Testing

Follow RED -> GREEN for each executable behavior:

- compiler unit test: an inconclusive result with explicit positive quality,
  critical-violation, cost, and latency credit contributes only cost/latency;
- adapter fixture: provider error without verifier becomes inconclusive and is
  excluded from quality;
- adapter fixture: recovered provider request plus final verifier reward keeps
  the task reward while counting the reliability exclusion;
- adapter fixture: a multi-decision single cell/tier credits exactly one
  representative decision;
- adapter fixture: multiple route cells or mixed actual tiers preserve the
  packet but withhold quality;
- matrix fixture: direct economy gates and balanced experiment gates produce
  literal expected rows;
- template test: balanced and strong are protected, strong remains escalation,
  and canonical compiler/evidence contracts validate;
- real BitRouter validation: generated subjects and results deserialize and
  pass the generic contract.

Finish with the focused suites, skill validation, template/config validation,
`cargo test --all-features`, Clippy with warnings denied, format, diff check,
schema checks, an explicit self-review report in this plan's ignored SDD
workspace, a clean worktree, and a normal push of the existing branch.

## Non-goals

- Publishing or activating a candidate policy.
- Running or resuming a paid benchmark.
- Adding Harbor or Terminal-Bench fields, errors, or enums to Rust/SDK schemas.
- Treating provider reliability as route quality.
- Broadcasting one terminal reward over requests.
- Guessing economy routes when the external analysis artifact is absent.
- Rewriting branch history or force-pushing.
