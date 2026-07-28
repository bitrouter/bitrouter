# Benchmark Methodology

This reference defines the experiment independently of any private run, AWS account, provider, model, or runner implementation.

## Contents

- [Question and evidence boundary](#question-and-evidence-boundary)
- [Fixed experimental topology](#fixed-experimental-topology)
- [Run classes and scale](#run-classes-and-scale)
- [Immutable identities](#immutable-identities)
- [Control policy](#control-policy)
- [Policy evolution](#policy-evolution)
- [Trials and retries](#trials-and-retries)
- [Tuning and held-out evaluation](#tuning-and-held-out-evaluation)
- [Concurrency and temporal drift](#concurrency-and-temporal-drift)
- [Allowed claims](#allowed-claims)

## Question and evidence boundary

Ask one causal question:

> Can BitRouter replace strong-model calls with cheaper-model calls while preserving Terminal-Bench task quality?

Answer it only when the same case/trial identities have:

- a fixed-strong quality and cost baseline;
- complete policy-round quality and cost;
- request-level model selections and decision reasons;
- exact trace, decision, usage, reward, and workflow-session attribution;
- authoritative cache-aware settlement;
- comparable harness, sandbox, protocol, and pricing inputs.

The result is the complete predeclared set of points for each model combination: `control+r1+r2`, optionally followed by r3. Freeze and report every targeted point. A cheaper round is not a success if quality falls outside the predeclared tolerance, and a high-quality round is not a routing-economics result if its cost evidence is incomplete.

## Fixed experimental topology

Use all of the following for benchmark evidence:

1. Terminal-Bench 2.1 tasks and verifier.
2. Harbor as the orchestration framework.
3. Terminus 2 as the neutral agent harness, avoiding a first-party Anthropic or OpenAI agent as the experiment controller.
4. One ephemeral AWS EC2 sandbox for each trial.
5. A separate central EC2 host running the pinned BitRouter daemon and the benchmark controller.
6. Private sandbox-to-daemon traffic; public network access only where package bootstrap or controller SSH requires it.
7. Request-level BitRouter traces, decisions, metering, reward attribution, and explicit workflow sessions.

Local Docker, direct model probes, and fixture tests are useful smoke evidence. They do not satisfy the benchmark gate because they omit the production EC2 lifecycle and cleanup behavior.

## Run classes and scale

| Class | Tasks | Trials | State reset | What it measures |
| --- | ---: | ---: | --- | --- |
| Non-evaluation canary | A task not in the scored manifest | 1 | Fresh | Infrastructure, agent, attribution, and cleanup only |
| Mechanism iteration | Short 13 or another predeclared 8-20 task slice | One trial per case | Fresh control/policy separation | Whether routing and learning work |
| Replicated mechanism | Predeclared slice or 89 tasks | At least 3 | Replicate the complete clean policy lineage | Paired uncertainty and path dependence |
| Public reproduction | 89 tasks | Five predeclared trials | As declared by the public protocol | Reproducible full Terminal-Bench result |

A teammate may choose approximately 20 tasks for broader iteration, but must publish the exact task manifest before observing results. The full dataset has 89 tasks. Changing task membership or trial count creates a different experiment identity.

In concise terms: an internal mechanism run may use one trial per case, while a public reproduction uses five predeclared trials for each of the 89 tasks.

## Immutable identities

Define a policy lineage with:

```text
benchmark version + exact task/trial manifest
Terminus 2 and Harbor revision/configuration
strong, balanced, and economy provider/model/protocol/credential classes
BitRouter source revision, binary checksum, and configuration
sandbox AMI, instance type, region, and network shape
policy-state lineage
price snapshot and source date
fixed concurrency, retry rule, and timeouts
ordered target groups for each model combination
```

Changing any item starts a new policy lineage. Give every lineage and round a new label, directory, port set, trace set, log set, and controller state. Never reuse a partial run directory or policy database.

Define the control key separately:

```text
benchmark version + exact case/trial identity
Terminus 2 and Harbor revision/configuration
fixed-strong provider/model/protocol/credential class
sandbox AMI, instance type, region, and network shape
```

Do not include the policy implementation commit in the control key. This separation lets multiple BitRouter policy revisions reuse the same immutable control without rerunning it.

## Control policy

Use a dedicated, clean fixed-strong database and no learning feedback. Keep tasks, trials, agent configuration, entry protocol, timeouts, sandbox shape, and pricing basis identical to policy comparison groups.

Maintain an append-only control catalog. Each record contains:

- a canonical control key and task/trial manifest hash;
- accepted and terminal-failed case/trial identities;
- harness, model, protocol, sandbox, and timestamp provenance;
- raw usage buckets and the pricing snapshot used at publication time;
- an artifact identifier and checksums.

For a control key, launch each case/trial identity at most once. Once the controller records `started`, retain its accepted outcome or terminal failure permanently. Never rerun it to repair quality, compensate for provider drift, or search for a better score. A preflight probe or explicitly non-evaluation canary is not a control case.

When prices change, reprice the archived raw usage under a new declared snapshot. Do not call the model again.

## Policy evolution

Use a clean policy database for r1 and share exactly that database with every later targeted policy round:

| Group | Database | Feedback | Interpretation |
| --- | --- | --- | --- |
| r1 | New clean policy DB | Apply only after strict acceptance | Cold exploration |
| r2 | Same policy DB | Apply only after strict acceptance | First adapted policy |
| r3, when targeted | Same policy DB | None needed for this evaluation lineage | Learned replacement and stability |

Freeze one ordered sequence before launch. Common sequences are `r1 -> feedback -> r2 -> feedback` and `r1 -> feedback -> r2 -> feedback -> r3`. Ending at r2 is a declared experimental boundary, not a post-hoc skip; persist the post-r2 policy snapshot and feedback marker before accepting that lineage. Do not omit a targeted round because an intermediate point is expensive or lower quality, do not continue after a rejected round, and do not add an unplanned r3/r4 to search for a favorable outcome.

In a multi-model matrix, target groups are per combination rather than global. For example, one already-running combination may finish r3 while all remaining combinations are frozen as `control+r1+r2`. This changes the questions those lineages answer but does not weaken any per-group acceptance gate.

Apply benchmark reward only when the group has complete quality, settlement, join, and cleanup evidence. Never apply reward to the control database, a held-out evaluation, a diagnostic-only artifact, or a rejected group.

Distinguish request reasons:

- an exploration trial is learning cost;
- a learned lock or promoted weak selection is replacement evidence;
- a static economy route is operator-authored behavior;
- a fallback is reliability behavior, not learned optimization.

## Trials and retries

A trial is a predeclared independent sample. A retry is an additional attempt caused by failure. They are not interchangeable.

Use Harbor retry count zero for mechanism validation. Within a lineage, never relaunch a started case/trial identity. Preserve its failure, cost, request IDs, and zero or missing verifier outcome according to the evidence contract.

If a public protocol permits a replacement after a proven transient infrastructure failure, assign a new explicit identity, retain the failed attempt, and report attempted trials and retry cost separately. Never overwrite the original or retry an immutable control for score improvement.

## Tuning and held-out evaluation

Executed policy rounds on the same tasks use verifier reward as an optimization signal. They measure adaptation, not unbiased generalization.

For a held-out claim:

1. Select tuning and held-out tasks before policy results are visible.
2. Learn only from tuning tasks.
3. Freeze and checksum the resulting policy database and configuration.
4. Clone that exact snapshot for each held-out attempt so held-out trials cannot teach one another.
5. Never apply held-out rewards.
6. Compare frozen policy and immutable control on the same held-out case/trial identities.

If the sample is too small for one holdout, predeclare cross-validation. Reset state for every fold and aggregate only untouched-fold results.

Benchmark verifier reward is an oracle unavailable in normal product traffic unless an equivalent production reward source exists. Describe experiments without such a source as reward-supervised policy optimization.

## Concurrency and temporal drift

Treat concurrency as a frozen lineage input. Establish explicit workflow-session attribution at concurrency 1, then test higher values with separate non-evaluation canaries. A cautious ladder may evaluate 3, 4, 6, and 8 in new identities; no historical result makes those values safe in another AWS account or provider environment.

Never raise or lower concurrency inside a targeted policy lineage. If a canary fails, diagnose it and start a new canary or lineage with a newly frozen value.

Because an immutable control is not rerun contemporaneously, record the time gap between it and policy groups. Send non-scoring strong-route sentinels around policy rounds to observe availability, latency, and rate-limit drift. Sentinel data explains temporal confounding but never mutates or replaces the control.

## Allowed claims

| Evidence | Allowed statement | Not allowed |
| --- | --- | --- |
| Direct probe | Endpoint/protocol works at that moment | Benchmark quality or cost |
| Non-evaluation canary | EC2, agent, attribution, settlement, and cleanup path works | Scored model comparison |
| One-trial short run | Routing/learning mechanism produced an observed point | Stable model ranking or public score |
| Replicated tuning run | Mechanism behavior with paired variation | Held-out generalization |
| Frozen held-out run | Generalization on the declared held-out sample | Broader production performance without external evidence |
| Full 89-task, five-trial public run | Reproducible Terminal-Bench result under the frozen configuration | Cash-spend claim when costs are only notional |

Keep runtime acceptance and product success separate. A perfectly assembled run can show that a policy is unstable; that is valid evidence, not a failed measurement.
