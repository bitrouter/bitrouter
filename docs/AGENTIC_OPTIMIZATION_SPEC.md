# Agentic Workflow Optimization

Status: implementation contract

## Product outcome

BitRouter can optimize an arbitrary agent workflow from its observable outcome
and routed request settlements. A new user can configure the loop during
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
bitrouter optimize publish
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
- `savings_first`: explore the eligible route key with the greatest settled
  cost and expose the measured trade-off for manual review.
- `custom`: use explicit version-controlled quality gates.

Latency is collected and displayed but is `observe_only`; it is not an
optimization objective in this version.

## Architecture

### Ownership boundaries

BitRouter owns:

- workflow process supervision;
- policy, decision, request, and settlement identity;
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
demand and pinned to an exact version. The resolved agent id, executable
version, concrete judge model, result schema, success-contract digest, and
generic-eval skill digest are pinned in the optimization lock.

Each evaluation uses a fresh session. The generic eval skill and protocol
reference are compiled into the BitRouter binary and supplied as immutable
context, so the flow does not depend on a project-local skill installation.
The workflow evidence packet is bounded, redacted, and supplied directly; the
judge does not need repository write or shell permission.

The judge route is independent of the candidate being measured. It either uses
a concrete BitRouter Cloud model or the detected agent's own direct
subscription, according to the pinned executor configuration. Judge traffic is
never included in workflow candidate cost.

### Experiment loop

One `optimize run` performs these steps:

1. Load and validate intent, lock, source config, and active policy lock.
2. Create a unique run directory under the BitRouter home.
3. Derive a minimal private daemon config with a unique loopback port, control
   socket, database, and the active policy lock.
4. Run the configured workflow without a shell while pointing common OpenAI,
   Anthropic, and BitRouter base-url variables at the private daemon.
   Fingerprint exact argv, its resolved executable, and referenced regular-file
   arguments before and after both variants.
5. Require at least one settled named-policy decision and build a redacted
   baseline evidence packet.
6. Select one eligible route key according to the qualitative preference.
7. Build a non-publishable experiment lock that differs only by mapping that
   key from its baseline tier to the configured economy tier.
8. Run the identical workflow command against a fresh private daemon and
   database using the experiment lock.
9. Evaluate baseline and candidate outputs independently against the same
   success contract. `inconclusive` is preserved and never converted to pass.
10. Import host-authored settlement metrics and agentic quality results into
    the source config's Eval Exchange, crediting quality only to the route key
    changed by the controlled experiment.
11. Freeze an immutable Eval snapshot and compile a v2 candidate from the
    active parent lock plus that exact snapshot.
12. Save the report, candidate, and lock transition. Never publish as part of
    `run`.

Long or failed workflow commands occupy only their own run. There are no hidden
retries. A non-zero exit remains quality evidence for the generic evaluator;
it is not mislabeled as a routing-infrastructure error. A timed-out,
source-drifted, or structurally ambiguous run is terminal and cannot produce a
publishable candidate. Users who need statistical power configure their
workflow command to run an eval suite; BitRouter does not secretly resample it.

### Evidence and credit

The private packet may contain bounded stdout, stderr, exit status, elapsed
time, and user-authored success criteria. The persisted Eval subject contains
only redacted evidence descriptors and SHA-256 digests.

Request cost and latency come from settled request subjects emitted by the
private daemon. The host submits those metrics with generic operational
authority. The agentic evaluator submits only `quality.pass` semantics.

When the candidate changes exactly one request key, quality credit may be
assigned to decisions for that key. Other decisions receive only their own
request-scoped cost and latency credit. If the observed candidate does not
preserve that single-variable condition, quality credit is withheld and the
run is inconclusive.

### Publication

`optimize review` shows:

- baseline and candidate pass/fail/inconclusive outcomes;
- exact settled cost and percentage delta when both sides have priced usage;
- observed latency as non-gating information;
- the one route-key change;
- evaluator, model, skill, contract, evidence, and policy digests;
- any admission, attribution, or comparability caveat.

`optimize publish` revalidates the candidate parent digest, optimization lock,
Eval snapshot, and source config. It then delegates to the existing atomic
policy publication path. No interactive prompt is required, but publication is
always a distinct command.

## CLI surface

```text
bitrouter optimize setup [options]
bitrouter optimize run [--config FILE]
bitrouter optimize review [--run ID] [--config FILE]
bitrouter optimize publish [--run ID] [--config FILE]
bitrouter optimize status [--config FILE]
```

Headless setup accepts exact argv rather than a shell program:

```text
bitrouter optimize setup \
  --workflow-command ./run-eval \
  --workflow-arg --case-set \
  --workflow-arg smoke.jsonl \
  --strong bitrouter:<strong-model> \
  --economy bitrouter:<economy-model> \
  --preference balanced \
  --yes
```

Interactive onboarding presents agentic review as the default evaluator. It
does not display unvalidated percentage promises. `custom` exposes exact
integer PPM gates in the version-controlled intent.

## Failure behavior

The optimizer fails closed when any of these are missing or inconsistent:

- active source config or named policy lock;
- distinct strong and economy models;
- evaluator executable or structured result;
- workflow settlement, policy decision, or pricing join;
- active/candidate command comparability;
- exact policy parent lineage;
- single-variable candidate mutation;
- Eval admission or snapshot integrity;
- private daemon cleanup.

Secrets are never written to intent, lock, reports, process arguments, or
version-controlled artifacts. Cloud credentials are resolved from the existing
credential store or environment and passed only through child-process
environment.

## Acceptance tests

The implementation is complete only when all of the following are proven:

1. Onboarding and headless setup create valid intent, policy, and lock files
   without overwriting existing user-owned files.
2. A fake ACP evaluator receives the embedded generic skill and returns a
   schema-validated opinion; malformed output gets one repair turn and then
   fails closed.
3. A deterministic mock workflow executes under baseline and one-key candidate
   private daemons, produces settled request evidence, and never mutates the
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
10. A real onboarding-to-second-cycle smoke test succeeds using BitRouter Cloud
    credentials and an installed Codex ACP evaluator, without task-data
    injection into routing.

## Implementation sequence

1. Add optimization intent/lock types, deterministic serialization, path
   resolution, validation, and reports with failing tests first.
2. Parameterize candidate compilation with quality-first, manual-review, and
   explicit custom gates while preserving the legacy compiler default.
3. Add the embedded-skill evaluator prompt, ACP execution, Cloud credential
   reuse, result validation, and fake-agent tests.
4. Add private daemon/workflow supervision, experiment-lock generation,
   settlement extraction, and cleanup tests.
5. Add Eval import, snapshot, compile, review, and publish orchestration with a
   full mock-provider integration test.
6. Wire `bitrouter optimize` and onboarding, then update CLI/skill references
   and generated schema artifacts where applicable.
7. Run the real Cloud/Codex experience twice, audit artifacts and secret
   boundaries, independently review the diff, and publish the implementation
   PR with the plan and evidence.
