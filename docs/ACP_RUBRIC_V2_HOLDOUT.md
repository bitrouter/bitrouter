# Rubric v2: fresh subscription-task comparison

Completed 2026-09-11 UTC. Following the [initial real-task pilot](ACP_SUBSCRIPTION_PILOT.md), four fresh constructed task families were executed through the local Codex subscription and the maintained ACP controller. This is actual model-generated coding evidence on controlled tasks. It is separate from naturally sampled historical usage, evaluator calibration and live routing benefit.

The [manifest](experiments/acp_rubric_v2_holdout.json) and [full local report](../../../studies/acp-rubric-v2-holdout-20260911/README.md) retain source/binary hashes, frozen records, sealed model references, both production-judge outputs, failures, metering and validation logs. Original pilot records and v1 results were not replaced.

## Method and observations

Tasks and starting artifacts were sealed before execution. Workers received ordinary coding instructions, without the rubric or target rewards. Coding used gpt-5.6-sol with low reasoning through openai-codex; adapter 1.10.0 and Codex CLI 0.153.4. Capture used the preserved v1 binary and independent first-stop checkpoints. A gpt-6-astra factual reference received the projected evidence without either rubric or comparator labels. Reference outputs were sealed before v1 and v2 production judges, both gpt-5.6-sol, evaluated each checkpoint sequentially.

| Fresh task | v1 result | v2 result |
|---|---|---|
| Review capacity boundary; fixes prohibited | Review resolution applicable, 1; total 1 | Review resolution N/A; total 1 |
| Edit cleanup; executable checks prohibited | Static read treated as verification=1; total 1 | Verification N/A; total 1 |
| Fix version boundary; prescribed environment lacks dependency | Verification=0; total 0.70 | Verification unknown; bounds 0.70–1.00 |
| Resolve explicit review finding; tests required | Verification=1, review resolution=1; total 1 | Both remain 1; total 1 |

The first two totals hide changes in obligation selection and denominator. The blocked check remains applicable and unknown, not N/A or a failing behavioral assertion. Its bounds are not a complete reward and do not pass the existing learner completeness gates.

All four checkpoints were gap-free. All 196 reference/comparator citations resolved, and the two versions retained identical checkpoint, prefix digest and ordered citation identity. These checks validate provenance, not semantic entailment. The v2 boundary projection removes nested identity/usage metadata while preserving canonical records and digests; task or tool business text can retain residual identity clues.

The reference judged all four requested deliveries supported, while explicitly identifying unavailable runtime verification in the third. Its broad repair-responsibility boolean also included ordinary implementation tasks, despite explanations saying there were no separate review findings. This ambiguity is retained; no mechanical rubric agreement or false-success metric is claimed. The revised prompt, anchors and projection were tested together, with one successful judgment per version and a fixed order; there is no isolated ablation or statistical superiority claim.

## Implementation and acceptance

The current contract versions are recorded-acp-quality-evidence-v2, coding-checkpoint-rubric-v2 and recorded-evidence-judge-v2. Post-hoc obligation selection distinguishes report-only review, actual finding-resolution responsibilities, forbidden execution, blocked validation, observed failures and stale checks. Verification facts remain prompt/anchor/explanation semantics, not new typed fields or a programmatic correctness proof.

Measurement and input digests fence version changes. Current aggregation rejects legacy labels/projections, and incomplete old-contract jobs retire before reserving another model request. An actual completed v1 job resumed through v2 returned unchanged: all 36 request IDs and session assessment history remained identical. Earlier committed assessments remain historical records.

Validation passed: 101 focused tests; 3,377 all-feature tests with 22 skipped; five doc tests with one ignored; all-feature/all-target Clippy with warnings denied; formatting and diff checks. These checks apply to the recorded v2 source hashes. Earlier native terminal campaigns remain separate prior-snapshot evidence.

The study issued 20 coding requests, four references, eight v1 judge attempts and four v2 judge attempts. Four v1 attempts failed with upstream connection errors and later succeeded; every attempt remains recorded. This is not evidence of a reliability improvement. All 36 requests used openai-codex with unknown charge status. Four failed calls also had unknown usage despite stored numerical zero fields. No USD cost or savings is inferred.

Evolution stayed off; there were no blocks, publications or learning updates. The isolated daemon was stopped. The headless execution shares the coding TUI controller but is not a new PTY campaign. Load/resume continuity, adapter termination-error representation, per-request causal credit, natural-history calibration and multi-model routing benefit remain separate work.
