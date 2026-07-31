# Generic Agentic Eval and Terminal-Bench short13 Design

Date: 2026-07-31

## Outcome

Ship a reusable `evaluating-bitrouter-routes` agent skill and use it to run one
new Terminal-Bench 2.1 short lineage with GPT-5.6-sol as the strong subscription
route and BitRouter Cloud DeepSeek V4 Pro as the economy route.

The run is mechanism evidence, not a stable public benchmark. It contains one
matching strong-only control and three policy rounds (`r1`, `r2`, `r3`), each
using the same frozen 13 tasks and one trial per task. Every transition is:

```text
fixed lock -> route and observe -> external eval -> admitted snapshot
           -> compile candidate -> explicit publish -> next fixed lock
```

The serving process never learns from SQLite rows during a request.

## Product boundary

BitRouter owns immutable subjects, result admission, snapshots, deterministic
candidate compilation, and atomic publication. It does not own the judge.

An evaluator may be a task-native verifier, a human, an agent, or an enterprise
private system. Each produces the same `EvaluationResult` contract through the
CLI or REST Eval Exchange. An evaluator may put a higher-scope subject when a
workflow spans multiple request subjects, but it may not edit or publish a
policy lock. Publication remains an explicit operator action.

The new skill teaches any compatible agent to perform that evaluator role. It
is generic: Terminal-Bench is its first real use, not part of its contract.

## Alternatives considered

### 1. Task-level eval packet plus generic skill (selected)

Construct one immutable task/episode subject from exact decision IDs and
redacted evidence digests, have the actual evaluator judge only the requested
dimensions, submit explicit per-decision credit, then verify admission. The
evaluator may be task-native, human, enterprise, or agentic; using the skill
does not by itself make the evidence agentic.

This preserves task-level outcome semantics without broadcasting one reward to
every turn. It also works for human and private evaluators because only the
evaluator adapter changes.

### 2. Score automatically created request subjects directly

This is simpler, but a single request rarely carries enough context to judge a
coding task. It encourages invented outcomes and full reward attribution to
each turn. It is retained only for genuinely request-local metrics such as
request success, latency, or cost.

### 3. Build a Terminal-Bench evaluator into BitRouter

This would make the first demo easy but would couple the product to one harness
and recreate the non-general eval problem. It is rejected.

## Generic eval workflow

1. Select or construct a subject at the smallest scope that contains the
   outcome: request, episode, or task.
2. Preserve exact policy digest, decision ID, request key, selected tier, and
   certificate-defined baseline tier. Never infer a counterfactual execution.
3. Redact evidence before hashing. Put only safe metadata and content-addressed
   references in the subject; keep raw private material at the evaluator.
4. Evaluate only `requested_dimensions`. Encode unsupported dimensions as
   absent, not zero. Use `inconclusive` when evidence cannot support a verdict.
5. For a multi-decision subject, provide explicit `decision_credit.metric_ids`.
   Credit only decisions for which the evidence supports causal attribution.
6. Submit a stable evaluator identity, rubric/config digest, evidence digest,
   evidence references, confidence, and idempotency key.
7. Require an `admitted` response before the result may influence compilation.
   `held_out`, `rejected`, and `disputed` remain evidence but do not train.
8. Stop at admission. Snapshot, compile, diff, and publish are operator work.

The skill will contain the short workflow in `SKILL.md` and exact field,
authority, CLI, REST, and failure semantics in one reference file. It will not
ship a second schema implementation or an evidence hashing script; BitRouter's
CLI/REST validation remains authoritative. BitRouter will add a local
`eval subject seal` operation so agents can calculate the canonical evidence
digest without reimplementing Rust serialization.

### Skill RED findings

Three independent agents solved multi-turn agentic, human, and private
enterprise cases without the skill. They understood the broad exchange, but
their procedures diverged at exactly the fragile seams the skill must bind:

| Observed baseline behavior | Required correction |
|---|---|
| All three hand-implemented canonical evidence hashing with different jq/Python recipes. | Provide one BitRouter-owned `subject seal` command and forbid hand-rolled canonicalization. |
| One named decisions `auto:cost`; another correctly identified the lock policy as `auto` and `cost` as a top-level variant. | Copy policy identity from router-authored decision refs; never derive it from the requested preset string. |
| Agents guessed equal or numeric multi-turn credit weights even when causal attribution was unspecified. | Require a versioned credit policy in evaluator config; unsupported attribution stays uncredited. |
| Human/private examples recorded arbitrary metrics that the current compiler does not consume. | Separate recordable metrics from route-affecting quality, cost, latency, and hard-violation semantics. |
| The private case discovered that automatic subjects use selected/static tier as baseline rather than the certificate comparator. | Fix runtime baseline propagation; never instruct adapters to rewrite a known router-authored baseline. |
| Procedures continued into snapshot/publish even though evaluation authority should end at admission. | Make the skill's output contract end with the admission result and a handoff packet for the operator. |

The baseline did not fail at the overall architecture; it failed through small,
high-impact inconsistencies. The minimal skill is therefore a positive output
contract plus exact current-command reference, not a long discipline document.

## Runtime correctness changes

### Baseline tier

The current lock runtime records `decision.static_tier` as `baseline_tier`.
For an explicit economy route, that incorrectly reports economy as its own
baseline even when the route certificate compares it with strong.

The runtime must pass certificate baselines into each named policy router. Eval
subjects and policy-decision records use the certificate baseline for explicit
routes and the policy default for unmatched/default routes. Selected/static
tier remains the actual route.

### Pretrained route ownership

The shipped `templates/auto-router` routes are described as migrated learned
routes but are marked `owner: operator`. Admitted negative evidence therefore
produces a blocking conflict instead of evolution.

These pretrained routes must be compiler-owned with a non-operator source so
the deterministic compiler can retain or demote them. Runtime mode still
controls publication; compiler ownership does not enable online mutation.

## Benchmark design

### Frozen task manifest

Use the established short13 set, in order:

1. `terminal-bench/regex-log`
2. `terminal-bench/log-summary-date-ranges`
3. `terminal-bench/nginx-request-logging`
4. `terminal-bench/openssl-selfsigned-cert`
5. `terminal-bench/fix-git`
6. `terminal-bench/git-multibranch`
7. `terminal-bench/filter-js-from-html`
8. `terminal-bench/sqlite-db-truncate`
9. `terminal-bench/circuit-fibsqrt`
10. `terminal-bench/write-compressor`
11. `terminal-bench/path-tracing-reverse`
12. `terminal-bench/configure-git-webserver`
13. `terminal-bench/extract-elf`

The harness is Terminal-Bench 2.1 through Harbor with neutral Terminus-2. The
central BitRouter daemon runs on the existing benchmark EC2 controller and each
trial receives one ephemeral, deleting EC2 sandbox. Retries are zero. The
initial maximum concurrency is three only after a current-source canary passes.

### Model groups

- Control: Terminus-2 requests explicit `openai-codex:gpt-5.6-sol`; explicit
  provider-qualified models bypass the preset policy.
- `r1`: Terminus-2 requests `@auto:cost` against the frozen pretrained lock.
- `r2`: exact `r1` artifacts are evaluated, admitted, frozen, compiled, diffed,
  and explicitly published; no other input changes.
- `r3`: repeat the same boundary using only accepted `r2` artifacts.

The Codex provider imports the local Codex subscription credential. The
DeepSeek provider uses the supplied BitRouter Cloud credential. Sandboxes see
only a scoped BitRouter virtual key, never either upstream credential.

### Generic agentic evaluation adapter

For each accepted trial, assemble a task subject from:

- task instruction digest;
- redacted Terminus trajectory digest;
- task-native verifier result and digest;
- exact request-ID to decision-ID join;
- active policy and model identities;
- actual cost and latency evidence when settled.

The task-native verifier remains authoritative for `quality.pass`. In this
lineage, the accepted fixed-strong control is an observed counterfactual for the
same task: a candidate failure is negative route evidence only when control
passed; shared failure is inconclusive. Credit is limited to one changed route
family, otherwise it is withheld. This does not infer an unexecuted baseline or
broadcast a task reward across requests.

An agentic judge is optional and consumes a redacted task packet locally only
when an additional rubric dimension or ambiguous-decision analysis is actually
requested. It cannot overturn a task-native hard failure and must identify
itself as `agentic`; the deterministic counterfactual adapter identifies itself
as `task_native`. The submitted result explicitly attributes each metric to the
decisions it can support. Ambiguous decisions receive no positive-weight
credit.

### Acceptance and lineage

Before a group can feed the next round, require all 13 terminal trial states,
exact task identities, valid Harbor artifacts, settled BitRouter requests,
strict request/decision joins, provider reconciliation, and complete EC2
cleanup. A failed or partial group is marked rejected and never becomes
training input. Consumed trial identities are never rerun.

Control and every round freeze these inputs in an append-only manifest:

- BitRouter, Harbor, Terminal-Bench, agent, and runner revisions;
- model/provider IDs and protocol;
- policy lock bytes and semantic digest;
- task configs and trial IDs;
- AWS account/profile, region, quota, instance type, tags, and concurrency;
- pricing source, capture time, and unknown-price treatment;
- evaluator ID/version/config digest and evidence snapshot root.

### Reporting

Report per group:

- task pass count/rate with exact task outcomes;
- strong/economy request and token mix;
- actual BitRouter Cloud spend;
- subscription usage separately from any notional GPT price;
- EC2 sandbox and central-controller cost;
- evaluator cost;
- total measured cost and explicitly unknown components;
- lock diff and certificate changes between rounds.

Do not combine actual and counterfactual cost into one unlabeled number. Because
the run is 13x1, conclusions are limited to mechanism evidence and observed
direction; no confidence or stable-ranking claim is allowed.

## Error handling

- Provider or harness canary failure: fix root cause before any scored trial.
- Missing exact join or unsettled request: reject the trial/group; do not infer.
- Eval digest mismatch or unauthorized metric: retain rejection evidence and
  correct the adapter with a new immutable result identity.
- Operator-owned conflict: do not force-publish; correct ownership or policy
  intent, compile a new candidate, and start a new lineage if scored work began.
- Publish CAS failure: keep active bytes, re-read the active parent, and compile
  again; never edit a candidate in place.
- Resource leak: stop progression and clean exact tagged instances, volumes,
  and interfaces before continuing.

## Verification

The PR gate includes focused eval/policy tests, skill RED/GREEN forward tests,
skill schema validation, template validation, formatting, Clippy, and the full
all-features test suite. The operational gate additionally includes direct
strong/economy sentinels, a real Terminus-2 EC2 canary, provider settlement,
strict artifact acceptance, and a final resource/secret audit.
