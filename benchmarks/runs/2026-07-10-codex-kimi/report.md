# Codex + Kimi Harbor Full Terminal-Bench v2.1 Run 20260710

Status: **complete; all four groups accepted and archived**

This document records the full Terminal-Bench v2.1 evolution run. Append results and failures here; do not replace or discard completed rounds.

## Goal

Run the complete 89-task Terminal-Bench v2.1 dataset with one attempt per task for:

1. fixed-strong control;
2. adaptive policy r1;
3. adaptive policy r2;
4. adaptive policy r3.

The primary mechanism question is whether later policy rounds replace strong-model calls with Kimi calls while preserving benchmark score and reducing total cost.

This is a same-task reward-supervised evolution run. It measures mechanism convergence, not held-out generalization. Public generalization evidence still requires a separate tuning/held-out protocol, which has not yet been run (see "Protocol and limitations" in the [benchmarks README](../../README.md)).

## Frozen Experiment

- Run ID: `codex-full-a4ce879-c3-20260710T123558Z`
- BitRouter branch: `feat/adaptive-policy-routing`
- BitRouter commit: `a4ce879172df56d4230d0904e7285f4f29d80469`
- Benchmark: Terminal-Bench v2.1, 89 canonical tasks; 88-task comparable set
- Attempts: 1 per task and group
- Agent harness: Harbor built-in Codex adapter
- Strong route: `openai-codex:gpt-5.5` via the Codex agent adapter
- Weak route: `bitrouter:moonshotai/kimi-k2.7-code`
- Concurrency: 3
- Sandbox: Harbor EC2 provider, `m7i-flex.large`, ephemeral/delete enabled
- Central node: `m7i-flex.large`, separate from agent sandbox runtime
- Strong imputed price: $5/M input, $30/M output
- Weak imputed price: $0.7125/M input, $3/M output
- Harbor retries: 0
- Timeout multiplier: 1.5

The central node was resized from four to two vCPUs so three two-vCPU sandboxes fit under the account's current eight-vCPU Standard On-Demand quota. The existing request for a 160-vCPU quota remains open.

## State Protocol

- Control uses a dedicated clean DB and never receives benchmark reward.
- Policy r1-r3 share one new policy DB.
- Each group gets separate trace, decision, usage, outcome, daemon-log, and Harbor-log files.
- Reward feedback is applied only after the just-finished policy group passes all artifact integrity gates.
- Intermediate quality/cost regression does not stop the predeclared r1-r3 lineage; infrastructure, provider, or evidence-integrity failures do.

## Required Evidence Per Group

- 88 benchmark outcomes from the comparable set;
- non-empty request traces;
- one usage row per trace;
- one policy decision per trace for policy groups;
- complete cost and reward joins;
- High session confidence for every request;
- no replay collision, visibility gap, or unknown IR state;
- no non-200 provider request or settlement recorder failure;
- no orphan EC2 sandbox.

## Local Run Assets

`runs/codex-full-a4ce879-c3-20260710T123558Z/`

This directory contains the frozen configs, remote runner, and analysis script. Accepted remote artifacts will be copied into the same directory after completion.

## Execution Log

- Preflight canaries passed for both `openai-codex:gpt-5.5` and `bitrouter:moonshotai/kimi-k2.7-code` with real streamed responses, trace capture, policy decisions, and usage records.
- The full runner started the control group at `2026-07-10T12:48:54Z`.
- Resizing the central node changed its public IP while the sandbox SSH security-group rule still allowed the old IP. The rule was updated to the new address; Harbor resumed the existing trials without discarding benchmark state.
- At `2026-07-10T13:52:32Z`, control had completed 14/89 tasks with 9 passes, 239 completed traces, 237 metering rows, two requests in flight, zero non-200 traces, and zero metering errors.
- At `2026-07-10T14:06:40Z`, control had completed 18/89 tasks with 11 passes, 330 completed traces, 328 metering rows, two requests in flight, zero non-200 traces, and zero metering errors. Recorded usage was 6,281,708 input and 247,449 output tokens, or approximately $38.83 at the frozen strong-model imputation price.
- At `2026-07-10T14:31:19Z`, control had completed 24/89 tasks with 14 passes, 418 completed traces, 417 metering rows, one request in flight, zero non-200 traces, and zero metering errors. Recorded usage was 8,001,460 input and 330,610 output tokens, or approximately $49.93 at the frozen strong-model imputation price.
- Codex occasionally stops consuming the stream after receiving the terminal event. These HTTP 200 requests are retained with BitRouter-estimated usage and will be reported separately from provider or benchmark failures.
- At `2026-07-10T14:54:22Z`, the control Harbor job had written 32/89 trial results and 30 verifier rewards. The two missing rewards were the already identified four-vCPU pre-agent failures; the third four-vCPU task had not run yet.
- At `2026-07-10T15:43:22Z`, the strong-route provider began returning `usage_limit_reached` 429 rate-limit responses. The limit reset at approximately `2026-07-10T17:18:45Z`. Thirty-seven control tasks that started in this interval exhausted the initial request plus five Codex reconnects without receiving usage; an earlier task had independently failed on an upstream 502. These 38 provider failures are invalid trials rather than model scores.
- The generalized recovery assets were validated with 11 local tests and real remote Harbor config resolution. They identify 41 control invalidations: 38 provider failures and the three four-vCPU pre-agent failures. Supervisor PID `36498` is waiting for original runner PID `3014` to exit and release the run lock; no live main-job config or policy state was replaced.
- `model-extraction-relu-logits` returned the same upstream 502 in the original run and 12 independent recovery sandboxes, including attempts separated by 15-minute backoff. Other tasks succeeded in the same provider windows. This is treated as a reproducible provider/task incompatibility rather than a score of zero. The task is excluded as N/A from all four groups before policy execution, leaving 88 identical comparable tasks. Every failed attempt remains quarantined and the exclusion is recorded in each merge manifest.

### Invalidation recovery

The complete registry cache contains 83 one-vCPU tasks, three two-vCPU tasks, and three four-vCPU tasks. `caffe-cifar-10`, `mcmc-sampling-stan`, and `rstan-to-pystan` cannot start their Docker Compose service on the two-vCPU `m7i-flex.large` sandbox. This happens before agent execution and is an infrastructure invalidation, not a benchmark score of zero.

The account's Standard On-Demand quota remains 8 vCPUs and the 160-vCPU increase request is still `CASE_OPENED`. The strong-route provider can also exhaust its time-window rate limit during a full group. To preserve completed work without accepting infrastructure or provider failures as benchmark scores, every group follows this recovery protocol:

1. Run the canonical task set at concurrency 3 on `m7i-flex.large`. Apply the same recorded provider exclusions to every group; this run excludes only `model-extraction-relu-logits`, yielding 88 comparable tasks.
2. Detect pre-agent failures and `usage_limit_reached`, upstream 429, or upstream 502 failures directly from Harbor results. Parse `resets_at` and wait automatically when the provider window has not reset.
3. Re-run four-vCPU tasks at concurrency 1 on `m7i-flex.xlarge`. Re-run other invalid tasks in batches of at most eight at concurrency 3 on `m7i-flex.large`, recomputing validity after every batch.
4. Select the first replacement trial that started the agent, has a numeric reward, and contains no provider invalidation. Build a merged Harbor directory with exactly one accepted trial for each of the 88 comparable task IDs.
5. Filter traces, policy decisions, and metering usage to the 88 accepted session keys. Preserve all rejected attempts and unfiltered evidence under `quarantine/`.
6. Require complete trace/usage/reward joins and all existing replay, session-confidence, provider, and recorder gates before accepting the group or applying reward feedback.

Agent executions that start normally and fail for task or agent reasons remain valid zero-score benchmark trials. Pre-agent infrastructure failures and explicit provider invalidations are replaced.

## Results

All values below use the frozen 88-task comparable set. Cost is normalized,
API-equivalent imputed cost, computed from measured token usage at the frozen
list prices above so the comparison across routes is reproducible at published
prices. It is a modeled figure, not a billing statement.

| Group | Passed | Score | Total requests | GPT-5.5 | Kimi | Kimi share | Cost | Cost vs control | Score vs control |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| control | 68/88 | 77.27% | 1,666 | 1,666 | 0 | 0.00% | $330.70 | baseline | baseline |
| r1 | 70/88 | 79.55% | 1,678 | 1,647 | 31 | 1.85% | $350.92 | +6.11% | +2.27 pp |
| r2 | 67/88 | 76.14% | 1,512 | 1,293 | 219 | 14.48% | $222.22 | -32.80% | -1.14 pp |
| r3 | 73/88 | 82.95% | 1,571 | 1,537 | 34 | 2.16% | $303.60 | -8.19% | +5.68 pp |

Cost per successful task was $4.86 for control, $5.01 for r1, $3.32 for
r2, and $4.16 for r3. Across the complete learning lifecycle, r1-r3 cost
$876.74 versus a $992.09 three-control counterfactual, a reduction of 11.63%.
The policy rounds produced 210 aggregate successes versus 204 for the control
counterfactual, and reduced cost per success from $4.86 to $4.17.

### Mechanism result

The core replacement hypothesis is supported in r2. It made 219 Kimi calls,
including 204 calls from learned locks and 15 new exploration trials. Compared
with control, strong calls fell by 373 (22.39%), total calls fell by 154
(9.24%), and cost fell by $108.48 (32.80%). The weak calls therefore replaced
strong calls rather than merely adding exploration on top. The one-task score
drop is within the noise that a single attempt cannot resolve.

r1 behaved as an exploration round: it added 31 Kimi trials, improved the raw
score by two tasks, but cost 6.11% more than control. r3 produced the highest
raw score and still cost 8.19% less than control, but it used only 34 Kimi
calls. It is a good quality/cost point, not evidence of monotonic routing
convergence.

### Evolution finding

The r2-to-r3 reduction from 204 learned-lock calls to seven exposes an important
policy limitation. Current workflow fingerprints are broad and shared across
many tasks, while adequacy is learned from binary task-level reward. A failed
task can therefore revoke confidence in a fingerprint that was useful in many
successful tasks. The policy then falls back to GPT-5.5 and explores the same
region again. The observed r2/r3 pattern is consistent with this global-key,
coarse-reward oscillation.

Before treating evolution rounds as a production optimizer, the next policy
experiment should add scoped or hierarchical adequacy estimates, retain
confidence with decay/hysteresis instead of binary lock reset, and attribute
reward more narrowly to the routed calls that plausibly affected the outcome.
A repeated-seed tuning/held-out run is then needed to distinguish policy effect
from one-attempt benchmark variance.

### Evidence integrity

- Every accepted group has 88 outcomes and complete trace/usage/reward joins.
- Session confidence is High for all 6,427 accepted traces; replay coverage is
  100%, with zero collision, visibility gap, unknown IR state, non-200 accepted
  request, or settlement-recorder failure.
- `model-extraction-relu-logits` is the sole provider exclusion. The original
  run and 12 independent recovery sandboxes reproduced the same upstream 502.
- r1 was rerun because an earlier resume path truncated trace/decision evidence
  before archive. The invalid artifact is quarantined and is not included here.
- One r2 Codex stream was received by the daemon but aborted by the client
  before provider settlement. Its request ID is explicitly allowlisted out of
  accepted cost evidence; no settled usage was discarded.
- The three four-vCPU tasks were recovered on `m7i-flex.xlarge`. Other pre-agent
  or provider-invalid trials were replaced under the frozen recovery protocol;
  normal verifier failures remain score zero.

### Archives

The derived evidence — per-request metering, policy decisions, benchmark
outcomes, policy learning state, summaries, and the cross-round analyzer output —
is committed directly under [`data/`](data/) and reproduces every value in the
results table. The raw HTTP capture (`traces.jsonl`) is withheld because its
request bodies carried provider account identifiers and auth tokens. See
[`data/README.md`](data/README.md) for the file layout, the withholding
rationale, and a one-liner that recomputes the headline pass counts.
