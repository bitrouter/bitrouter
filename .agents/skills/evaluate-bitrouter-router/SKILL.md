---
name: evaluate-bitrouter-router
description: Design, run, or review BitRouter routing and shadow-classifier evaluations, including dataset splits, bake-offs, calibration, abstention, and evidence for quality or cost claims. Do not use for ordinary registry catalog edits.
---

# Evaluate the BitRouter router

Keep evaluation separate from runtime policy authority. A candidate classifier
may produce semantic evidence; deterministic risk rules, signed policy, and
runtime guards remain authoritative unless the requested design explicitly
changes that boundary.

## Orient

- Read `docs/architecture/agentic-optimization.md` when the work changes how
  trace evidence becomes a routing policy.
- Read `docs/architecture/eval-lockfile.md` when the work changes frozen policy
  or evaluation artifacts.
- For learned or shadow classifier experiments, read
  [the research protocol](references/shadow-classifier.md) before designing the
  dataset or interpreting results.

## Workflow

1. State the hypothesis, candidate, baseline, routing metric, cost metric, and
   rejection threshold before running the experiment.
2. Keep training, calibration, and sealed evaluation groups disjoint by task or
   session—not merely by individual request.
3. Preserve raw predictions, provenance, configuration, and digests needed to
   reproduce the result. Treat checked-in fixtures as contract tests, not as a
   sufficient production dataset.
4. Compare against the deterministic baseline and earlier accepted candidates.
   Measure calibration, abstention/OOD behavior, slice regressions, resource
   cost, and downstream routing outcomes where applicable.
5. Report uncertainty and failed thresholds. Do not convert a directional
   experiment into a product claim.

## Verify

Run the repository's focused classifier, policy-lock, replay, or bake-off tests
for the surface changed. If Rust source changed, also run the complete checks
required by `AGENTS.md`. Keep large experimental outputs outside the repository
unless they are deliberately reviewed contract fixtures or committed evidence.
