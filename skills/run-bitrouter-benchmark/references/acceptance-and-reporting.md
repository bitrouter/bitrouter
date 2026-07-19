# Acceptance and Reporting

This reference defines when a benchmark group is usable and how to report it without overstating quality, savings, or reproducibility. Apply it independently to control and every policy round.

## Contents

- [Evidence inventory](#evidence-inventory)
- [Strict group gate](#strict-group-gate)
- [Four-bucket settlement](#four-bucket-settlement)
- [Exact joins and runtime validity](#exact-joins-and-runtime-validity)
- [Reward and reliability semantics](#reward-and-reliability-semantics)
- [AWS cleanup gate](#aws-cleanup-gate)
- [Metrics and formulas](#metrics-and-formulas)
- [Actual and notional economics](#actual-and-notional-economics)
- [Uncertainty and comparison](#uncertainty-and-comparison)
- [Required report](#required-report)
- [Scenario walkthroughs](#scenario-walkthroughs)
- [Registry and public archive](#registry-and-public-archive)

## Evidence inventory

An independently reviewable group contains:

- frozen private manifest plus sanitized manifest;
- task/trial manifest and hash;
- BitRouter, Harbor, Terminus 2, config, patch, and binary provenance;
- append-only controller events and process records;
- Harbor config, logs, and one result per case/trial;
- daemon logs, request traces, policy decisions, metering rows, provider receipts, outcomes, and workflow-session evidence;
- reconciliation record and strict bundle result;
- cleanup monitor and authenticated AWS residue query;
- feedback output and policy snapshots for accepted r1/r2 only;
- per-group and per-case summary, checksums, and secret-scan result.

Do not treat a summary JSON or Harbor exit code as a substitute for the underlying evidence.

## Strict group gate

Accept a group only when every row below is true.

| Gate | Required evidence |
| --- | --- |
| Manifest | Frozen input hash matches the launched configuration |
| Attempts | Expected case/trial identities equal controller identities; no duplicate `started` event |
| TrialResult | Exactly one complete TrialResult per expected identity, verifier reward present, no unexplained exception |
| Runtime | All started cases are `terminal_valid`; runtime-invalid count is zero |
| Requests | Stable request IDs are unique and equal across expected trace/settlement membership |
| Decisions | One policy decision per policy request where the pinned schema requires it |
| Settlement | Every request is authoritative `computed` or authoritative `not_charged` |
| Cost join | Every trace matches exactly one usage/charge row; no unmatched trace or usage row |
| Reward join | Every trace maps to exactly one intended task outcome; no unmatched outcome |
| Session join | Every request maps to the exact Harbor `session_key` with High confidence |
| Replay | Offline reconstruction covers every policy request and agrees with online decisions |
| Cleanup | Observed sandbox peak respects frozen concurrency; final instances, volumes, and interfaces are zero |
| Secrets | Sanitized archive contains no live credential or private key material |

Record the terminal status as one of:

- `accepted`;
- `rejected_runtime`;
- `rejected_settlement`;
- `rejected_join`;
- `rejected_cleanup`;
- `rejected_manifest`;
- `rejected_safety_limit`.

A rejected group may still produce a diagnostic report. It must not drive policy feedback, a cost claim, or the next round.

The cost join, reward join, and session join are exact set-and-identity checks, not best-effort ratios.

## Four-bucket settlement

Every request row contains numeric values for all four fields:

- `uncached_input_tokens`;
- `cache_read_tokens`;
- `cache_write_tokens`;
- `output_tokens`.

Zero is valid only when it is an observed/authoritative value. Missing is not zero. Record separately billed reasoning tokens and fixed/request charges in additional fields without folding them silently into output tokens.

For route $r$, define prices per token as $p_{u,r}$ for uncached input, $p_{cr,r}$ for cache read, $p_{cw,r}$ for cache write, $p_{o,r}$ for output, and $p_{reason,r}$ for separately billed reasoning. The reconstructed variable charge is:

$$
C_r = n_u p_{u,r} + n_{cr} p_{cr,r} + n_{cw} p_{cw,r}
      + n_o p_{o,r} + n_{reason} p_{reason,r}
$$

Add an explicitly sourced fixed charge only when the provider's billing contract requires it.

Price precedence is:

1. an explicit valid route-specific cache price;
2. when the pinned product behavior declares it, the same route's valid base input price;
3. otherwise `unknown`.

Never borrow a price from another route/model, replace an invalid base price with zero, infer cache splits from prompt length, or estimate missing provider usage from historical averages.

`computed` requires reconstructable usage and price evidence or an authoritative provider charge. `not_charged` requires an authoritative terminal receipt proving no charge. `pending` and `unknown` reject the group after the frozen reconciliation budget expires.

## Exact joins and runtime validity

Let $T$, $U$, $D$, and $R$ be the request-ID sets in traces, usage, decisions, and request-level reward attribution. For a policy group, require:

$$
T = U = D = R
$$

For control, omit $D$ only when the frozen control schema intentionally emits no policy decision. Require uniqueness in every set and reject extra rows as well as missing rows.

For each trace, require:

- one authoritative settlement row;
- one routing decision when applicable;
- one explicit workflow session equal to the outcome `session_key`;
- one task outcome selected by that session;
- an HTTP/runtime outcome consistent across logs and artifacts.

Do not join by overlapping timestamps when trials run in parallel. A time window may bound a scan but cannot establish membership.

TrialResult completeness means the verifier ran, reward is present, and exception metadata is empty or an explicitly accepted typed terminal condition. A typed `TerminalSessionEnded` may still yield a valid TrialResult when the environment remains reachable and the verifier completes. An environment disconnect, missing verifier result, corrupt output, or generic agent exception is runtime-invalid.

## Reward and reliability semantics

Maintain two evidence planes:

| Plane | Key | Positive/negative signal |
| --- | --- | --- |
| Provider reliability | Provider + model + credential class + region/protocol | Timeout, 429, 5xx, successful completion, half-open probe |
| Semantic adequacy | Workflow/task capability state + model transition | Verifier reward after a successfully completed, attributable request |

Do not let task success give semantic success to an earlier timed-out/ejected weak request. Do not let a provider timeout permanently prove that the model lacks task capability. Require successful transport, attributable outcome, and authoritative settlement before writing positive semantic evidence.

Report exploration trials, learned selections, static route choices, escalations, and fallbacks separately. Savings attributed only to fallbacks are reliability behavior, not learned replacement.

## AWS cleanup gate

After every group, independently query exact run tags and tracked IDs for:

- EC2 sandbox instances in all non-terminated states;
- EBS volumes created for those instances;
- elastic network interfaces created for those instances.

Acceptance requires an observed peak no greater than frozen concurrency and a final monitor tail of zero. Save empty query results, UTC timestamps, region, and redacted identity proof.

Do not infer cleanup from Harbor logs. Do not search by broad project tag alone. A retained central host must be excluded only by an explicit role/instance identity in the frozen manifest.

## Metrics and formulas

Report per group and per task/trial:

- passed attempts, total attempts, and score;
- total/strong/balanced/economy request counts;
- request counts by decision reason;
- all token/charge buckets by route;
- actual cost, notional cost, and unpriced/unknown count;
- cost per successful attempt;
- latency and provider error counts;
- failed tasks and paired control outcomes.

For binary or scalar verifier rewards $q_i$ across $N$ predeclared trials:

$$
\text{score} = \frac{1}{N}\sum_{i=1}^{N} q_i
$$

For accepted comparable policy and control totals:

$$
\text{cost delta \%} =
\left(\frac{C_{policy}}{C_{control}} - 1\right) \times 100
$$

$$
\text{strong-call delta \%} =
\left(\frac{N_{strong,policy}}{N_{strong,control}} - 1\right) \times 100
$$

$$
\text{cost per success} =
\frac{C_{group}}{\text{successful trial count}}
$$

For learning-lifecycle economics through round $K$:

$$
C_{policy,1:K} = \sum_{k=1}^{K} C_{policy,k}
$$

$$
C_{control,1:K} = K \times C_{frozen\ control}
$$

$$
\text{cumulative delta \%} =
\left(\frac{C_{policy,1:K}}{C_{control,1:K}} - 1\right) \times 100
$$

Keep steady-state savings and cumulative adaptation cost in separate rows. A cheap r3 does not erase expensive or low-quality r1-r2 behavior.

## Actual and notional economics

Label every cost series:

- **actual cost:** an authoritative provider/platform charge attributable to the request;
- **notional cost:** raw usage multiplied by a cited list or registry price;
- **subscription counterfactual:** usage priced as if billed per token even though marginal cash spend may be zero;
- **incomplete:** any pending, unknown, or unpriced request.

Do not add actual and notional costs into one total. Subscription-backed control costs are normally notional routing-economics comparisons unless the provider exposes an attributable charge. Publish both when both are useful, with source date and assumptions.

## Uncertainty and comparison

Compare only identical case/trial identities. If a historical quota/runtime incident permanently consumed some control identities, restrict the paired comparison to the exact intersection and report excluded identities and reasons.

For replicated runs, compute paired task/trial deltas and a predeclared 95% interval, such as a bootstrap over task/trial pairs. Requests are not independent benchmark samples. Because policy state is path-dependent, include independent clean-database lineages in stability analysis.

Report the time gap between immutable control and policy. Sentinel telemetry can describe provider drift; it cannot turn a historical control into a contemporaneous randomized sample.

## Required report

Publish sections in this order:

1. hypothesis, run class, allowed claim, and predeclared quality/cost tolerances;
2. tuning/held-out role and reward availability assumptions;
3. immutable experiment tuple, source/config hashes, and EC2 topology;
4. control artifact resolution and case/trial intersection;
5. one table containing control and every predeclared policy round;
6. paired per-task/trial quality, cost, and request deltas;
7. strong/balanced/economy decisions and learned-replacement evidence;
8. four-bucket settlement and exact join audit;
9. runtime, provider, retry, concurrency, and cleanup evidence;
10. steady-state and cumulative adaptation economics;
11. discarded/failed attempts, limitations, and temporal drift;
12. go/no-go decision and artifact/checksum locations.

Never publish only the best round. Never delete a failed attempt from denominators. Keep runtime acceptance separate from whether the policy met the product target.

## Scenario walkthroughs

### Internal short13 walkthrough

Freeze 13 tasks, one trial per case, an existing verified central host, one BitRouter/Harbor/Terminus source tuple, and fixed concurrency. Resolve a matching accepted control and launch zero control cases. Create a fresh policy database, run r1-r3 in sequence, strictly accept every round before feedback, and report all 39 TrialResults plus exact joins and cleanup. Describe the result as mechanism evidence.

### Teammate 20-case walkthrough

Freeze the teammate's explicit AWS identity, region, provider secret sources, strong/balanced/economy models, source commits, prices, approximately 20 predeclared tasks, trial count, and concurrency. Reuse a control only when its complete key matches; otherwise launch only absent control identities once. Prove the new environment with a non-evaluation canary and staged concurrency canary before scored work. Publish configuration and evidence without exposing secrets.

### External full 89-task walkthrough

Provision a new central host from public instructions, freeze all 89 tasks and five predeclared trials per task, and build from published BitRouter/Harbor commits. Create or resolve the immutable control, run the declared policy/evaluation design, accept every group strictly, and publish sanitized raw evidence, checksums, price provenance, limitations, and registry fields. This is the path for an externally reproducible result.

## Registry and public archive

When contributing a Terminal-Bench 2.1 result to BitRouter's model registry, use the repository's current schema. At the time of this contract it includes:

```yaml
benchmarks:
  terminal_bench_2_1:
    accuracy: 0.0
    cost_per_task: 0.0
    time_per_task: 0.0
    measured_by: bitrouter
    harness: terminus-2
    config: operator-declared-config
    as_of: 2026-01-01
    source_url: https://example.invalid/public-evidence
```

Replace example values with verified results. Omit optional metrics that cannot be proven. `source_url` is required for third-party `measured_by` values and is recommended for first-party evidence as well. Check current registry documentation before editing generated catalogs.

The public archive should contain raw messages/tool calls only when policy and privacy permit, plus sanitized configs, request/session IDs, usage buckets, decisions, outcomes, receipts, controller events, cleanup proofs, reports, and checksums. Run a secret scan before upload and again on the downloaded release. Record archive byte count and verify every published checksum.
