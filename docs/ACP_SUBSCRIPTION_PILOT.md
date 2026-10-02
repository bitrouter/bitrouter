# Real Codex subscription ACP pilot

Follow-up: the [rubric v2 fresh-task comparison](ACP_RUBRIC_V2_HOLDOUT.md)
implements responsibility and boundary-projection corrections and tests four new
families. The v1 measurements below remain unchanged.

Completed 2026-09-11. The user explicitly authorized generating controlled coding
sessions in this worktree with the local Codex subscription. This is a separate
track from the historical-data protocol. It contains real model-generated work
and observed tool execution, not fixed upstream responses, natural usage, human
labels or evidence of routing savings.

The [result manifest](experiments/acp_subscription_pilot_v1.json) identifies the
source snapshot, byte-identical rebuilt binary, raw artifact directory, content
hashes, model-reference judgments, comparator results and operational failures.
The [full local report](../../../studies/acp-subscription-pilot-20260910/README.md)
links every frozen packet, output, checkpoint and original task artifact.

Five independent task families produced six coding turns: pagination repair with
five passing tests; an edit-only slug change with execution forbidden; a YAML
change whose required test command failed during import because PyYAML was absent;
a retry-delay change followed by a new requirement without another test run; and
a review-only exact-stock finding. An earlier unsupported-model startup is retained
separately. Its ACP stop was end_turn and its CLI exit was zero despite no coding
work, demonstrating why neither signal can serve as task success.

Coding used gpt-5.6-sol through openai-codex. Seven frozen units were reviewed by
an independent gpt-6-astra request with no tools. Reference outputs were sealed
before scalar and production-rubric comparisons, both using gpt-5.6-sol with
provider-default generation settings. The maintained ACP adapter was 1.10.0 and
the worker Codex CLI 0.153.4. The headless run surface shares the TUI controller;
this pilot does not constitute a new interactive terminal campaign.

| Primary case | Scalar score | Rubric lower–upper |
|---|---:|---:|
| Pagination repair | 1.00 | 1.00–1.00 |
| Execution forbidden | 1.00 | 0.85–0.85 |
| Missing dependency | 0.98 | 0.70–0.70 |
| Retry first prefix | 1.00 | 1.00–1.00 |
| Review only | 1.00 | 0.778–1.00 |

Both methods matched the reference overall outcome on 4/5 primary families.
The scalar method had one success disagreement relative to the model reference;
the rubric method abstained on the review-only case. There was one applicability
disagreement among 30 rubric selections, one applicable-item score abstention,
and a score MAE of 0.0536 on 14 mutually scored applicable items. All returned
citations resolved within their frozen packets. These are descriptive agreements
with a fallible model reference, not accuracy guarantees or probability calibration.

The reference raised a potential YAML-null versus empty-document obligation that
both comparators missed. It explicitly recorded the interpretation as ambiguous.
The rubric's lower overall score in that case came from failed validation, so
agreement on the outcome does not establish agreement on the defect or cause.
The reference also assigned a literal unresolved-finding score to review-only work
while judging overall delivery successful. Keep these disagreements visible;
do not silently replace the sealed labels with a more convenient reference.

Actionable findings for the next implementation revision:

- Bind review-resolution applicability to an actual repair/acceptance obligation;
  finding and reporting a defect can itself complete review-only work.
- Separate task fulfillment from verification availability, attempted execution,
  observed result, final-artifact coverage and responsibility for an environment
  limitation. Forbidden or impossible checks cannot by themselves diagnose a bad
  model choice.
- Check semantic obligation coverage after rubric selection; JSON structure does
  not make a weaker judge discover all missing behavior.
- Preserve provider/task termination errors independently of normal ACP stops.
- Address nested model metadata in projected session-boundary results: model_usage
  remained visible, so this study is not fully blind to model identity.
- Keep the load/resume continuity gap explicit until continuity is verifiable.
  The second retry prefix had such a gap; the original head-37 content remained
  byte-identical after append to head 52.
- Request associations remain tool_to_request_unresolved. Checkpoint diagnosis
  does not prove which individual request caused a defect.

All seven production judge jobs eventually completed. There were 34 coding
requests (two failed) and 32 review requests: seven reference, fifteen scalar
(eight failed), ten rubric (three failed). Failed attempts were sealed before
retry, not removed. Successful median elapsed times were approximately 98.1,
19.5 and 60.9 seconds respectively; interrupted wall time is not isolated model
latency. All 66 request charge statuses were unknown. Legacy numerical zero
subtotals must not be interpreted as free usage or USD savings.

Evolution remained off, with no blocks, publications or automatic learning
updates. This corpus supplies an evaluator pilot, not multi-model bandit evidence:
there is one successful coding model and no randomized arm probabilities.
Re-evaluating these cases after changing rules is debugging; testing generalization
requires fresh task families. Historical natural-usage validation remains separate.
