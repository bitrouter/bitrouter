# Recorded ACP evidence review protocol

Status: prepared; no eligible historical ACP dataset is available and no
reference labels or comparator measurements have been produced. Controlled
native-worker test recordings are excluded from this historical review.

A later user instruction explicitly authorized a separate controlled real-model
collection track. The [Codex subscription pilot](ACP_SUBSCRIPTION_PILOT.md) now
contains executed tasks, sealed model references and comparator measurements.
That authorization applies to the separate pilot; it does not turn generated
controlled sessions into naturally sampled historical usage.

The [rubric v2 follow-up](ACP_RUBRIC_V2_HOLDOUT.md) uses four additional
constructed task families, with factual references sealed before old/new
production-judge comparisons. It remains part of that separate controlled track.

This protocol specifies the natural-history version of the first claim in
[ACP checkpoint evolution](ACP_EVOLUTION_SPEC.md). The later user-authorized
controlled pilot is a separate completed data-acquisition alternative; this
unavailable historical corpus is not a prerequisite for that pilot's completion.
The user authorized the
assistant to produce the reference review. Those labels must be described as
model-generated reference judgments, never human labels or an execution oracle.

## Dataset and evaluation units

Use existing app-owned ACP canonical recordings. Export immutable stopped
prefixes into an isolated study directory without changing the operator's
configuration, feedback selection, mode or source database. Record the source
snapshot, checkpoint ID, head, prefix digest, capture completeness and family
relationships. Do not recreate missing history from private worker logs or
generate new coding sessions to fill this study's dataset.

Select one checkpoint per native session for the primary comparison using an
explicit cutoff and the latest recorded stop at or before it. Freeze the
selection rule and manifest before inspecting comparator outputs. Record every
excluded item and its reason. Failed, interrupted and incomplete sessions must
not be dropped merely because they are hard to score: their unknown dimensions
and capture gaps are part of the evaluation. Empty/unreadable evidence cannot
yield a usable total score. Forks and multiple sessions in a known family remain
a cluster, not independent observations. A separate revision analysis may use
multiple prefixes, but must not inflate the primary sample count.

Generate packets through the production evidence projection and record its
version. Hide route choices, model prices, experiment arms and prior evaluations
from quality reviewers. Preserve original task requirements and contradictory
evidence. Tool names or user text can reveal harness/model clues; document this
residual unblinding rather than promising complete anonymity. Anonymized study
IDs map to canonical identities in a separate manifest.

## Reference first, comparators second

1. Give the reference reviewer only frozen evidence and the versioned rubric
   definitions. It records applicability, known/unknown scores, reasons and
   canonical citations for every criterion, plus evidence-supported material
   failures. Requirements are inferred from the already recorded prefix; no
   task-specific target is created before the coding work.
2. Seal reference outputs, the packet manifest and reviewer instructions with
   content hashes before producing any comparator output. Record reviewer
   identity as available, review time and limitations. Do not invent a precise
   served-model identifier when it is unavailable.
3. Run all comparators on the same frozen packets. Record exact prompts, model
   configuration, tool/library versions, output status and input/output digests.
   The scalar and rubric judges use the same configured model so the comparison
   does not conflate rubric structure with model strength.
4. Unseal for paired analysis. Preserve disagreements and counterexamples. A
   later reference correction gets a new version and a sensitivity analysis;
   it cannot silently replace the sealed primary result.

The reference may distinguish recorded execution facts from overall task
fulfillment. A passing test command is not proof that all requested behavior
works, and an assistant's claim that tests passed is not a test result. The
reference shares potential model errors with the evaluated judges; agreement
with it does not establish agreement with an independent human or ground truth.

## Comparators

| Comparator | Inputs and output | Main question |
|---|---|---|
| Recorded dialogue/evidence heuristics | A frozen, documented rule set over recorded statements and tool statuses; overall outcome indication and unknowns | How much does a simple baseline miss or falsely accept? |
| Scalar judge | The same evidence packet; an overall anchored score, unknown status, reasons and citations | Does decomposing evaluation improve on a single judgment? |
| Production rubric judge | Production applicability selection, criterion scores, citations and fixed aggregation | Does the deployed structured evaluation preserve obligations and detect failures? |

Heuristic rules and scalar anchors must be written and hashed before comparator
execution. They are not tuned on disagreements in the primary study. The scalar
baseline has no per-criterion output; do not invent such scores for a criterion
comparison. Invalid outputs, exhausted retries and missing responses remain in
coverage and operational-failure reporting.

## Measurements and limits

Report the number of checkpoints and known families, rubric/template coverage,
capture gaps and score availability for every method. Present denominators and
unknown counts alongside every metric; excluding unknown reference labels from
a particular error calculation does not remove them from the dataset.

- For rubric versus reference, report applicability confusion and score error
  where both have a usable applicable score. Report disputed applicability and
  abstentions separately instead of converting them to zero or success.
- For all three comparators, compare the overall supported outcome with the
  sealed reference. Freeze the common decision threshold before reference
  review or comparator execution and report the
  missing-value bounds for rubric totals. Any threshold adjustment belongs to a
  separate sensitivity analysis, not the primary comparison.
- Report false-success and missed-failure counts **relative to the model
  reference**, with examples and original citations. Reference-unknown items
  cannot establish either a true success or a definitive false positive.
- Record citation validity, unsupported positive verification claims, omitted
  obligations and confusion between introducing, discovering and fixing a
  problem. Do not interpret a diagnostic association as causal route credit.
- Pair comparisons on the same evidence. Split any later tuning/test sets by
  known family; if uncertainty intervals are reported, resample families rather
  than individual checkpoints. Family grouping addresses known dependence; it
  does not prove independence or representative sampling.

Anchored quality scores are not automatically probabilities. Do not label their
agreement or mean absolute error as probabilistic calibration. Brier scores or
reliability plots require a separately defined probability prediction and an
appropriate outcome label; see the probabilistic forecast setting in
[Gneiting and Raftery's scoring-rule analysis](https://sites.stat.washington.edu/raftery/Research/PDF/Gneiting2007jasa.pdf).
A small or selected corpus supports descriptive
results, not a general false-success guarantee. Sample adequacy and any stopping
rule must be documented before making a confirmatory claim.

Track observed judge requests, retries, known usage, priced cost, unknown cost
and latency for each method. This study adds no product-level judge budget cap.
Study execution boundaries and error handling are operational controls, not a
claim that missing or failed inference was free. Whole-feature routing savings
cannot be inferred from an evaluator comparison on logged outcomes.

## Deliverables and completion boundary

The completed study requires a dataset/cutoff manifest, frozen reviewer and
comparator instructions, sealed model-reference labels, raw outputs from all
three comparators, a paired analysis with costs and unknowns, and a cited
disagreement review. Record the source snapshot and all content hashes together.
Do not fill missing artifacts with synthetic results or claim the study complete
from this protocol alone. Until actual historical ACP evidence is available,
reference agreement, evaluator accuracy and historical false-success rates
remain unmeasured.
