# Full89 Targeted Matrix and Recoverable Provider Gates Design

**Status:** Approved for implementation on 2026-07-22.

## Context

The Terminal-Bench 2.1 full89 run uses Terminus 2 through Harbor, an EC2-hosted BitRouter daemon, and ephemeral EC2 sandboxes. The Sol + Kimi lineage is already running its final `r3` group. The remaining Terra, Fable, Sonnet, and Opus lineages no longer need `r3`; they need `control`, `r1`, and `r2` only.

Claude Code subscription quotas are model-specific. A `429` from Fable 5 must not be treated as evidence that Sonnet 5 or Opus 4.8 is unavailable. Individual benchmark cases can also encounter stable provider or network security policy refusals. Those cases must not cause unbounded recovery waves, but they must remain visible in the official 89-case score.

## Alternatives Considered

### 1. Operator-only rotation

Keep the runtime unchanged and document that the operator stops after `r2`, tries another Claude model after a `429`, and manually excludes policy-blocked cases. This is the smallest change, but it leaves acceptance behavior dependent on shell history and operator memory. It cannot prove that a lineage stopped at the intended group or that a skipped case met a stable refusal rule.

### 2. Target-aware runtime with independent model gates and a skip ledger

Give each combination an explicit ordered group target, persist provider readiness per model, and represent stable security-policy skips as immutable evidence. This preserves strict joins for all executed requests, prevents a Fable `429` from blocking Sonnet or Opus, and provides reproducible official and runnable-only scores. This is the selected design.

### 3. General matrix DAG scheduler

Build a persistent work queue that schedules all combinations, provider quotas, recovery waves, and group dependencies. This would be useful for a benchmark service, but it is unnecessary for the current single-operator run and would add new failure modes during an active experiment.

## Frozen Target Matrix

| Combination | Required groups | Completion marker |
|---|---|---|
| Sol + Kimi | `control`, `r1`, `r2`, `r3` | `LINEAGE_ACCEPTED` after `R3_ACCEPTED` |
| Terra + Kimi | `control`, `r1`, `r2` | `LINEAGE_ACCEPTED` after `R2_ACCEPTED` and `R2_FEEDBACK_APPLIED` |
| Fable + Kimi | `control`, `r1`, `r2` | same as Terra |
| Sonnet + Kimi | `control`, `r1`, `r2` | same as Terra |
| Opus + Kimi | `control`, `r1`, `r2` | same as Terra |

The already-started Sol `r3` identity is unchanged. No accepted group is rerun. `r1` and `r2` still apply feedback only after strict group acceptance, and the post-`r2` feedback snapshot remains part of lineage acceptance even when `r3` is not required.

## Component Design

### Combination target groups

`Combination` owns an immutable `target_groups` tuple. The global set of supported groups remains `control`, `r1`, `r2`, `r3` so existing artifacts and CLI commands remain readable. Preparation, input validation, `next_group`, and `run_lineage` operate on the selected combination's target tuple instead of assuming four groups.

New prepared run metadata records `target_groups`. Existing prepared Terra, Fable, Sonnet, and Opus roots can retain unopened `r3` configuration files, but runtime validation ignores them and the launcher refuses to execute them. This avoids rewriting frozen artifacts merely to remove an unused group. `LINEAGE_ACCEPTED` records exactly the target groups, their acceptance marker hashes, and any case skips. A direct `run-group --group r3` for a three-group lineage fails before daemon or sandbox creation.

### Per-model Claude provider gates

Provider readiness is keyed by the exact combination and provider model identity, not by `provider_family`. The three Claude keys are:

- `claude-code:claude-fable-5`
- route target `claude-code:claude-opus-4.8`, with provider model identifier `claude-opus-4-8`
- `claude-code:claude-sonnet-5`

A readiness probe is executed through the same BitRouter configuration and credential path that the group will use. Its result is appended to a model-specific JSONL ledger without storing credentials, request bodies, or response bodies. The record contains the combination, requested model, provider model identifier, timestamp, outcome class, HTTP status when available, and a sanitized retry hint.

Outcomes have these meanings:

- `ready`: the exact model can proceed.
- `rate_limited`: the exact model is deferred; zero sandbox identities are consumed.
- `policy_refused`: the probe reached the provider but cannot authorize a benchmark launch; zero sandbox identities are consumed.
- `transient_error`: the model remains pending and is not treated as a quota decision.

A `rate_limited` result for one Claude model does not create or update a family-wide cooldown. The operator advances another model's lineage. Results from one model are never substituted into another model's lineage.

### Stable security-policy skip ledger

Skipping is allowed only for a single case that cannot produce a valid TrialResult after both:

1. its original attempt, and
2. one immutable replacement attempt.

Both attempts must independently contain typed evidence for the same stable class: `provider_policy_refusal` or `network_security_policy_denied`. `429`, generic `5xx`, connection failures, environment-start failures, agent timeouts, missing usage, and untyped errors are not skippable security evidence.

If a typed refusal already produces a verifier-scored TrialResult, that result remains the accepted attempt and the case is not skipped. A skip exists only when the refusal prevents a valid TrialResult.

Each approved skip is an immutable JSON document at `case-skips/<group>/<case-id>.json` containing:

- schema version, combination, group, case and task identifiers;
- the two attempt identities and terminal states;
- the normalized refusal class;
- hashes and paths of the supporting trace/controller/Harbor evidence;
- creation time and runtime source/binary identity.

The document contains no prompt, response body, bearer, API key, or IAM credential. Once written, later recovery planning treats the case as terminal and never schedules another replacement for it.

### Acceptance and scoring

For every group:

$$
N_{accepted} + N_{security\_skipped} = 89
$$

All non-skipped cases still require exactly one accepted TrialResult. Executed requests retain the existing exact trace/decision/usage/session sets, five token buckets, strict cost join, strict reward join, and EC2 cleanup gates. A skipped case creates no synthetic TrialResult, request, usage row, reward row, or model output.

Two quality views are emitted:

$$
Q_{official} = \frac{\sum_{i \in accepted} reward_i}{89}
$$

$$
Q_{runnable} = \frac{\sum_{i \in accepted} reward_i}{N_{accepted}}
$$

`Q_official` is the primary reported Terminal-Bench score, so skips count as zero and cannot inflate quality. `Q_runnable` is diagnostic and is always labeled with its denominator. Cost and token totals include only real executed requests. Reports list every skipped case and evidence class.

## Data Flow

1. Resolve the combination and its `target_groups`.
2. Verify IAM profile, frozen source/binary, dataset, Harbor, credentials, and zero residue.
3. Run a readiness probe for the exact provider model.
4. If ready, execute the next required group with the frozen capacity of four.
5. Select valid attempts. When the original attempt has a typed security refusal and no valid TrialResult, run exactly one confirmation replacement before evaluating the stable security-skip rule. Non-security runtime or settlement failures retain the existing explicit recovery process and are not silently converted into skips.
6. Postprocess actual requests and enforce all strict joins.
7. Accept the group only when accepted TrialResults plus valid skip documents cover all 89 tasks.
8. Apply feedback after accepted `r1` and `r2`.
9. Write `LINEAGE_ACCEPTED` when the last target group and required feedback marker exist.

## Failure Handling

- Readiness `429`: persist the exact-model result, consume no case identity, and try another model lineage.
- Transient readiness failure: persist it separately; do not infer quota exhaustion or write a permanent rejection.
- Runtime or settlement failure: preserve current fail-closed behavior and recover only the exact affected cases or postprocess stage.
- Stable security refusal: permit one replacement, then create a skip only if both attempts have matching typed evidence and neither has a valid TrialResult.
- Ambiguous or missing skip evidence: fail closed and do not accept the group.
- SSH or monitoring failure: do not restart the central tmux job; restore management access and read the append-only event ledger.
- Cleanup residue: lineage remains unaccepted until instance, EBS, and ENI counts are all zero.

## Test Strategy

Unit and contract tests must prove:

- Sol targets four groups and every other combination targets three.
- preparation and validation create and inspect only the target groups.
- `next_group` stops after post-`r2` feedback for three-group lineages and after `r3` for Sol.
- a direct non-target group launch fails before starting a daemon or consuming a case identity.
- Fable `429` does not block Sonnet or Opus readiness.
- model gate ledgers contain no credential fields or raw response bodies.
- one typed refusal is insufficient for a skip; two matching typed refusals permit it.
- `429`, `5xx`, timeout, and missing-usage failures never permit a security skip.
- a valid TrialResult takes precedence over refusal evidence.
- recovery planning excludes only cases with valid immutable skip documents.
- group acceptance requires accepted plus skipped to equal 89 while keeping strict joins over every real request.
- official and runnable-only score denominators are correct.
- existing Sol control/r1/r2/r3 artifacts remain readable and the active Sol r3 flow is unchanged.

After local tests, the updated driver is deployed to the central EC2 host and its exact hash is recorded. A zero-sandbox dry run validates each three-group lineage boundary before any new case identity is consumed. Real runs keep fixed concurrency four.

## Completion Evidence

The benchmark phase is complete only when:

- Sol has accepted `control`, `r1`, `r2`, and `r3` plus `LINEAGE_ACCEPTED`;
- Terra, Fable, Sonnet, and Opus have accepted `control`, `r1`, and `r2` plus `LINEAGE_ACCEPTED`;
- every accepted group covers all 89 tasks through accepted TrialResults and explicitly listed security skips;
- all real requests pass strict cost/reward/session joins and five-bucket accounting;
- all group run IDs have zero instance, EBS, and ENI residue;
- final reports contain both score denominators, costs, routing mix, token buckets, skip evidence, frozen source/binary/config identities, and recovery provenance;
- the cost-optimization status documents, reusable benchmark skill, and PR #717 reflect the final method and results.
