---
name: run-bitrouter-benchmark
description: Use when planning, launching, resuming, auditing, or reproducing a BitRouter Terminal-Bench 2.1 benchmark with Harbor, Terminus 2, and AWS EC2, including short policy iterations, one-time controls, model comparisons, or public full runs.
---

# Run BitRouter Benchmark

## Core rule

Treat the benchmark as a fail-closed experiment, not a best-effort load test. Keep the validated method fixed, make environment-specific inputs explicit, and accept no quality or cost claim until trial, routing, settlement, attribution, and cleanup evidence all agree.

This skill is an operational document. It does not supply a runner, infrastructure module, or historical configuration. Inspect the selected BitRouter and Harbor revisions, adapt commands to their current interfaces, and freeze the resulting inputs before spending.

## Start here

1. Read [configuration.md](references/configuration.md). Collect names and locations of credential sources, never secret values in chat or artifacts. Produce a frozen run manifest and a redacted review copy.
2. Read [methodology.md](references/methodology.md). Classify the run and state exactly what its result may claim.
3. Before any AWS mutation or benchmark launch, read [operations.md](references/operations.md) completely and execute its gates in order.
4. After every group, read [acceptance-and-reporting.md](references/acceptance-and-reporting.md), independently validate the evidence, and record an accepted or rejected terminal result.
5. Use [qna.md](references/qna.md) when a symptom matches a known failure. Do not improvise around a failed gate.

## Classify the run

| Class | Typical scale | Trials | Valid use |
| --- | --- | ---: | --- |
| Mechanism iteration | short 13 or a predeclared small slice | 1 per case | Debug routing, learning, metering, and runtime behavior |
| Replicated experiment | predeclared slice or full set | At least 3 per case | Estimate paired variation across clean policy lineages |
| Public reproduction | full 89-task set | 5 per case | Publish a reproducible Terminal-Bench result with provenance |

Never describe a one-trial tuning run as a public model score. Never describe r3 on tasks that taught r1-r2 as held-out generalization.

## Preserve the fixed rail

Keep these invariant unless deliberately starting a different benchmark method:

- Terminal-Bench 2.1 dataset and verifier;
- Harbor with the neutral Terminus 2 agent;
- a separate central BitRouter daemon EC2 host;
- one ephemeral EC2 sandbox per trial, deleted after the trial;
- immutable case/trial identities and append-only controller/control records;
- request-level traces, policy decisions, four-bucket usage, rewards, and explicit workflow sessions;
- strict settlement, join, and AWS residue gates.

Configure AWS identity, account, region, network, instance shapes, source revisions, models, providers, secret sources, prices, task manifest, trial count, concurrency, timeouts, and central-host provision/reuse mode for each environment.

## Execute the lifecycle

1. **Freeze.** Record every input, source commit, checksum, task/trial identity, price snapshot, stop limit, retry rule, path, port, and resource tag before launch.
2. **Authenticate.** Select one explicit AWS identity, prove it with STS, and propagate the same selection to the controller and every Harbor subprocess. Fail before touching EC2 if identity is ambiguous.
3. **Preflight.** Validate configs through the pinned software, check quota immediately before each batch, prove network reachability, run provider sentinels, and verify that all run-scoped paths are new.
4. **Prepare the central host.** Provision or reuse it under the same source, health, isolation, credential, and network gates.
5. **Canary.** Run a predeclared non-evaluation trial through the real Terminus 2 and EC2 path. Require complete attribution and zero resource residue.
6. **Resolve control.** Reuse a matching accepted immutable control. Launch absent control identities once only; a started identity is consumed even if it terminates unsuccessfully.
7. **Run policy.** Freeze the exact target groups for every model combination before launch, start a fresh policy database, keep it across the targeted policy rounds, and run only those rounds in order. Apply feedback only after the preceding round is strictly accepted; a lineage targeted as `control+r1+r2` still records accepted r2 feedback before completion and must not invent r3.
8. **Settle and assemble.** Reconcile by stable request ID, wait for authoritative terminal charges, build the evidence bundle, and reject missing or ambiguous rows.
9. **Clean up.** Audit instances, volumes, and network interfaces by exact run tags after every group. Preserve cleanup evidence even for rejected groups.
10. **Report.** Publish every predeclared round, failure, raw usage bucket, price source, actual/notional distinction, checksum, and limitation. Never select only the best round.

## Hard stops

Stop the current lineage, preserve partial evidence, and clean up when any of these is true:

- the AWS identity, source revision, binary checksum, task manifest, or price snapshot differs from the frozen manifest;
- quota, daemon health, provider sentinel, sandbox bootstrap, or session canary fails before a batch;
- a group has a missing TrialResult not covered by the narrowly validated security-policy skip contract, a runtime exception, duplicate request ID, incomplete settlement, weak session attribution, or unmatched join;
- exact-tag resource cleanup does not reach zero;
- a predeclared spend or severe quality stop limit is crossed.

Do not apply feedback after a rejected round. Do not relaunch a started identity, rerun a control to improve its score, estimate unknown cost, delete failed attempts, or change concurrency inside a lineage.

## Quick reference

| Need | Read |
| --- | --- |
| Experimental meaning, controls, trials, held-out design | [methodology.md](references/methodology.md) |
| Required inputs, AWS/IAM, source/model/price checklist | [configuration.md](references/configuration.md) |
| EC2/Harbor/Terminus lifecycle, resume, settlement, cleanup | [operations.md](references/operations.md) |
| Strict gates, calculations, report and registry fields | [acceptance-and-reporting.md](references/acceptance-and-reporting.md) |
| Diagnosing known benchmark failures | [qna.md](references/qna.md) |
