# Indexed resource reader: maintained Claude failure

Source/binary: the 112-file manifest published at `a316e745`, before the
local archived-execution inspector. Run ID
`6370ffe1-d3c9-4316-b7e5-bee791c14ba9`, owned session 52028.
The original bandit, cohort size, sample minimums, 30-second processing
deadline and 2,400-second campaign bound remain unchanged.

Claude is terminal failure after 1,294.160 seconds. Its last periodic report
was trial 160, baseline 141/challenger 19, still Exploring. The next failure
was in `reading learning snapshot` at the original processing deadline.
The last returned snapshot described 162 trial sessions, cost present,
quality and latency absent for the current observation, and no rollback.
The worker was active and its last completed pass reported a judge job
error code `judge_execution_or_submission_failed`, zero judge completions
and zero publications. This code does not reveal the failing substage.

The snapshot log captures the complete Claude result and an explicitly
nonterminal Codex observation. It is not the combined run's final log.
Codex was subsequently confirmed live by the same session handle and owned
process 19768. Its opened canonical database was queried read-only for
aggregate row/job status: ten enrollments and nine completed judge jobs,
no failed/running job at that observation. Those counts are not a
performance comparison, adoption result or assurance of later success.
The active combined log remains separate and must not be committed as final.

The current reader's SQL selection is bounded and its integrity/index
regressions passed, but this result disproves that the reader change alone
completed maintained publication/rollback acceptance. It does not prove a
specific alternative cause. Source inspection confirms the production
judge path deliberately persists one sanitized generic failure code to
avoid storing provider response fragments. Further diagnosis must obtain
safe operation-stage evidence (preparation, execution, durable response,
submission/reconciliation or lease), preserve original request identities
and fees, and avoid exposing response text or inventing success.

Independent audit found another open G13/F09 issue: Goal UI event lines,
root artifact references and the renderer/writer's full logical/physical
documents still grow across a long Goal. Directly truncating those vectors
would break document offsets and conceal access to old committed inputs.
A bounded persistent-history browser and safe off-screen writer retention
are needed; the new archive inspector only bounds the currently opened
archive's references. No UI-history retention gate is marked complete.

Final combined result: 1 passed / 1 failed, exit 100, 3,520.577 seconds. Codex passed in 2,226.416 seconds: 234 trials (204 baseline / 30 candidate), eight monitor turns, adoption, quality rollback, restored baseline with Automatic enabled and stopped worker were verified under the original limits. The combined log is now terminal. Its earlier live observations above remain historical. Controlled model/judge replies establish native mechanism evidence, not real provider quality or Goal acceptance.
