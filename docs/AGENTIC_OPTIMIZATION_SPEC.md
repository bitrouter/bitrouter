# Agentic Workflow Optimization

Status: implementation contract

## Product outcome

BitRouter can optimize an arbitrary agent workflow from its observable outcome
and daemon-metered model requests. A new user can configure the loop during
onboarding, run the same workflow under a baseline and a one-variable routing
candidate, inspect measured quality and cost, and explicitly publish the
candidate. The online router remains independent of task data.

The default quality evaluator is BitRouter's generic agentic-evaluation
protocol. ORI and task-specific harnesses are optional evaluator providers, not
core dependencies.

## User contract

The primary flow is:

```text
bitrouter init
bitrouter optimize run
bitrouter optimize review
bitrouter optimize publish --enable-adaptive
```

`bitrouter init` can create two version-controlled files after the user opts
into workflow optimization:

- `bitrouter.optimize.yaml`: human-authored intent, workflow command, success
  contract, route ladder, evaluator choice, and qualitative trade-off.
- `bitrouter.optimize.lock.yaml`: resolved evaluator/runtime identities,
  content digests, active policy lineage, and latest candidate state.

Private workflow output and model replies are stored under the BitRouter home,
not in the source repository. The lock contains only identities, digests, and
aggregate measurements.

The onboarding trade-off choices are qualitative:

- `quality_first`: explore low-impact route keys and require no observed
  quality regression before recommending a candidate.
- `balanced`: explore the most frequently used eligible route key and expose
  the measured trade-off for manual review.
- `savings_first`: explore the eligible route key with the greatest normalized
  showback cost and expose the measured trade-off for manual review.

All three profiles require the candidate workflow's single agentic evaluation
to pass and its normalized showback cost to improve. They are search-order
preferences, not statistical quality-loss promises. Case-level quality budgets
remain a future extension once BitRouter has a host-verifiable task denominator.

Latency is collected and displayed but is `observe_only`; it is not an
optimization objective in this version.

## Architecture

### Ownership boundaries

BitRouter owns:

- workflow process supervision;
- policy, decision, request, and metering identity;
- private experiment daemon and database lifecycle;
- evidence hashing and redaction;
- Eval Exchange subjects, results, admission, and snapshots;
- candidate compilation, review, publication, and rollback.

The evaluator owns only a structured quality opinion:

```json
{
  "verdict": "pass | fail | inconclusive",
  "confidence": "high | medium | low",
  "critical_failure": false,
  "evidence_refs": ["workflow-output"],
  "reason": "bounded explanation"
}
```

The evaluator cannot author cost, policy digests, decision identities,
idempotency keys, or publication state. The trusted host constructs the final
`EvaluationResult`.

### Evaluator execution

The first supported executor is an installed ACP coding agent selected during
onboarding. `codex-acp` is preferred when Codex is installed; otherwise
`claude-acp` is selected when available. Direct use of the detected agent's
own subscription is the default; Cloud judging is an explicit opt-in. The
maintained Codex adapter is `@agentclientprotocol/codex-acp`, installed on
demand. Its exact package version and registry integrity, plus the resolved
Codex/Claude runtime version and executable digest, are pinned and rechecked.
The concrete judge model, result schema, success-contract digest, and
generic-eval skill digest are pinned in the optimization lock as well.

Each evaluation uses a fresh session. The generic eval skill and protocol
reference are compiled into the BitRouter binary and supplied as immutable
context, so the flow does not depend on a project-local skill installation.
The workflow evidence packet is bounded, redacted, and supplied directly; the
judge does not need repository write or shell permission.

The ACP adapter and installed Codex/Claude runtime are a trusted executor
boundary, not an OS sandbox: they can use the user's own subscription and
global agent configuration. BitRouter removes unrelated inherited credentials,
uses a dedicated cwd, denies ACP tool permissions, and never treats the judge
as an authority for cost or publication. Users who do not trust the installed
runtime should not select it as an evaluator.

The judge route is independent of the candidate being measured. It either uses
a concrete BitRouter Cloud model or the detected agent's own direct
subscription, according to the pinned executor configuration. Judge traffic is
never included in workflow candidate cost.

Every baseline/candidate tier route is executed by the private BitRouter
daemon. A tier is a provider-qualified daemon route, not a separate execution
backend: for example `openai-codex:gpt-5.6-sol` uses the daemon's Codex
subscription auth applier, while
`bitrouter:deepseek/deepseek-v4-flash-0731` uses the same daemon's BitRouter
Cloud OAuth auth applier. The workflow child receives only a loopback endpoint
and local sentinel credential. Known coding-agent executables such as Codex and
Claude also receive their catalog-owned argv/env routing adapter; a known
adapter that cannot prove loopback routing fails closed.

The cost objective is normalized showback, not necessarily cash spend. Metered
providers use their configured cache-aware prices. Flat-rate subscriptions can
pin an API-equivalent schedule in the version-controlled intent through
`normalized_price_overrides`; their actual marginal cash charge remains
unknown. Missing usage or pricing is never interpreted as zero.

### Experiment loop

One `optimize run` performs these steps:

1. Load and validate intent, lock, source config, and active policy lock.
2. Create a unique run directory under the BitRouter home.
3. Derive a provider-neutral private daemon config from the source provider,
   model, registry, and auth semantics, with a unique loopback port, control
   socket, database, and the active policy lock.
4. Create two detached Git worktrees from one frozen source manifest, overlay
   the same dirty/untracked files, and include any explicitly declared ignored
   dependencies or fixtures (`workflow.inputs`). Run the configured workflow
   without a shell, preserving user argv boundaries after any catalog-owned
   routing prefix, while pointing common OpenAI, Anthropic, Gemini, and
   BitRouter base-url variables at the private daemon. Apply the shared harness
   catalog adapter when the executable is Codex or another known agent.
   Fingerprint exact argv, its resolved executable, and referenced regular-file
   arguments before and after both variants.
5. Require at least one metered named-policy decision with complete usage and
   normalized-price evidence, then build a redacted baseline evidence packet.
6. Select one eligible route key according to the qualitative preference.
7. Build a non-publishable experiment lock that differs only by mapping that
   key from its baseline tier to the configured economy tier.
8. Run the identical workflow command against a fresh private daemon and
   database using the experiment lock.
9. Evaluate baseline and candidate outputs independently against the same
   success contract. `inconclusive` is preserved and never converted to pass.
10. Import host-authored normalized metering metrics and agentic quality
    results into the source config's Eval Exchange, crediting quality only to
    the route key changed by the controlled experiment.
11. Freeze an immutable Eval snapshot and compile a v2 candidate from the
    active parent lock plus that exact snapshot.
12. Save the report, candidate, and lock transition. Never publish as part of
    `run`.

Long or failed workflow commands occupy only their own run. Baseline and
candidate workflow identities are never retried. The evaluator may use one
bounded schema-repair turn, which does not relaunch the workflow. A non-zero
exit remains quality evidence for the generic evaluator;
it is not mislabeled as a routing-infrastructure error. A timed-out,
source-drifted, or structurally ambiguous run is terminal and cannot produce a
publishable candidate. Users who need statistical power configure their
workflow command to run an eval suite; BitRouter does not secretly resample it.

### Evidence and credit

The private packet may contain bounded stdout, stderr, exit status, elapsed
time, and user-authored success criteria. The persisted Eval subject contains
only redacted evidence descriptors and SHA-256 digests.

Request cost and latency come from metered request subjects emitted by the
private daemon. The host submits normalized showback with generic operational
authority. The agentic evaluator submits only `quality.pass` semantics.

When the candidate changes exactly one request key, the workflow-level quality
outcome and workflow-level normalized cost/latency delta are causally credited,
with exact weights, only to decisions for that treatment key. They are not
represented as per-request measurements. If the observed candidate does not
preserve that single-variable condition, credit is withheld and the run is
inconclusive.

### Publication

`optimize review` shows:

- baseline and candidate pass/fail/inconclusive outcomes;
- exact normalized showback cost and percentage delta when both sides have
  complete priced usage;
- observed latency as non-gating information;
- the one route-key change;
- evaluator, model, skill, contract, evidence, and policy digests;
- any admission, attribution, or comparability caveat.

`optimize publish` revalidates the candidate parent digest, optimization lock,
Eval snapshot, and source config. It then delegates to the existing atomic
policy/config publication and daemon reload path. A frozen project requires
the explicit first-publication consent `--enable-adaptive`; setup never changes
that safety mode. The transition is idempotently
recoverable if the process exits after the policy write but before the
optimization-lock compare-and-swap. No interactive prompt is required, but
publication is always a distinct command. `optimize rollback` restores an
archived policy and changes the optimization lock's active digest in the same
recoverable workflow, keeping the next experiment's parent lineage usable.

## CLI surface

```text
bitrouter optimize setup [options]
bitrouter optimize resolve [--config FILE]
bitrouter optimize run [--config FILE]
bitrouter optimize review [--config FILE]
bitrouter optimize publish [--run ID] [--enable-adaptive] [--config FILE]
bitrouter optimize rollback DIGEST [--config FILE] [--socket PATH]
bitrouter optimize status [--config FILE]
```

Headless setup accepts exact argv rather than a shell program:

```text
bitrouter optimize setup \
  --workflow-command ./run-eval \
  --workflow-arg --case-set \
  --workflow-arg smoke.jsonl \
  --workflow-input .venv \
  --strong openai-codex:gpt-5.6-sol \
  --economy bitrouter:deepseek/deepseek-v4-flash-0731 \
  --normalized-price openai-codex:gpt-5.6-sol=5,0.5,6.25,30 \
  --preference balanced
```

Interactive onboarding presents agentic review as the default evaluator. It
does not display unvalidated percentage promises. `review` is intentionally
latest-only; editing intent/contract is reconciled with `optimize resolve`,
which starts a fresh lineage.

Controlled execution is Unix-only in this version. Windows setup fails before
creating files until Job Object process-tree cleanup is implemented.

## Failure behavior

The optimizer fails closed when any of these are missing or inconsistent:

- active source config or named policy lock;
- distinct strong and economy models;
- evaluator executable or structured result;
- workflow metering, policy decision, or pricing join;
- active/candidate command comparability;
- exact policy parent lineage;
- single-variable candidate mutation;
- Eval admission or snapshot integrity;
- private daemon cleanup.

BitRouter never intentionally writes credentials to intent, lock, reports,
process arguments, or version-controlled artifacts. Provider credentials are
resolved by the private daemon through the same native auth appliers used in
normal service, including Codex subscription and BitRouter Cloud OAuth; they
are not forwarded to the workflow child. Workflow output is untrusted: known
token forms and the exact values of recognized sensitive environment variables are
redacted before evaluator/report persistence, but users must still keep secret
values out of eval output and treat the selected ACP runtime as trusted.

## Acceptance tests

The implementation is complete only when all of the following are proven:

1. Onboarding and headless setup create valid intent, policy, and lock files
   without overwriting existing user-owned files.
2. A fake ACP evaluator receives the embedded generic skill and returns a
   schema-validated opinion; malformed output gets one repair turn and then
   fails closed.
3. A deterministic mock workflow executes under baseline and one-key candidate
   private daemons, produces normalized metering evidence, and never mutates the
   active policy during `run`.
4. The imported Eval snapshot compiles a candidate with agentic certificates
   and exact parent/evidence digests.
5. Review reports measured quality and cost while treating latency as observed
   only.
6. Publish atomically promotes only the reviewed candidate and stale lineage is
   rejected.
7. A second cycle starts from the published lock and can evolve another route.
8. Cleanup leaves no private daemon, socket, or child process.
9. The full Rust test, Clippy, formatting, distribution, and repository hygiene
   checks pass.
10. A real onboarding-to-second-cycle smoke test succeeds with a Codex
    subscription strong tier and BitRouter Cloud OAuth economy tier, both
    proven to traverse the private daemon, without task-data injection into
    routing.

## Implementation sequence

1. Add optimization intent/lock types, deterministic serialization, path
   resolution, validation, and reports with failing tests first.
2. Parameterize candidate compilation with quality-first and manual-review
   search preferences while preserving the legacy compiler default.
3. Add the embedded-skill evaluator prompt, ACP execution, evaluator credential
   reuse, result validation, and fake-agent tests.
4. Add private daemon/workflow supervision, experiment-lock generation,
   normalized metering extraction, and cleanup tests.
5. Add Eval import, snapshot, compile, review, and publish orchestration with a
   full mock-provider integration test.
6. Wire `bitrouter optimize` and onboarding, then update CLI/skill references
   and generated schema artifacts where applicable.
7. Run the real Cloud/Codex experience twice, audit artifacts and secret
   boundaries, independently review the diff, and publish the implementation
   PR with the plan and evidence.
