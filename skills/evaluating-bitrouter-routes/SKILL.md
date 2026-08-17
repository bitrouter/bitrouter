---
name: evaluating-bitrouter-routes
description: Use when evaluating BitRouter route decisions or Eval Exchange subjects, including Harbor or Terminal-Bench runs, task-native verifiers, human reviewers, private enterprise evaluators, agentic judges, or genuinely uncategorized evaluator sources.
---

# Evaluate BitRouter Routes

Evaluate outcomes outside BitRouter's serving path. Produce an immutable result
and stop after BitRouter reports its admission status; an operator owns
snapshots, candidate compilation, diffs, and publication.

Read [the Eval Exchange reference](references/eval-exchange.md) before forming
a subject or result. It is the exact current wire and authority contract.

For Harbor or Terminal-Bench artifacts, also read
[the Harbor adapter reference](references/terminal-bench-harbor.md). Use its
script and fixed taxonomy; do not copy benchmark parsing or exception names into
BitRouter code or SDK schemas.

## Classify the evaluation

Choose scope from the observable outcome boundary:

| Evidence boundary | Scope |
|---|---|
| One request-local outcome | `request` |
| Bounded multi-request workflow or conversation | `episode` |
| Externally defined task identity plus terminal task or verifier outcome | `task` |

Choose `evaluator.kind` from the actual source:

| Evaluation source | Kind |
|---|---|
| Task-native verifier | `task_native` |
| Human reviewer | `human` |
| Private enterprise evaluator | `enterprise` |
| Agentic judge | `agentic` |
| Genuinely uncategorized evaluator | `generic` |

## Build the evaluator packet

1. Copy every decision's `decision_id`, `policy`, `request_key`,
   `selected_tier`, `baseline_tier`, and `policy_digest` from router-authored
   evidence.
2. Redact evidence before it leaves its private source. Retain raw messages,
   tool arguments, code, and evaluator output with the evaluator; place safe,
   content-addressed evidence items in the subject.
3. List only dimensions the evaluator was asked to judge. Leave unsupported
   dimensions absent. Use `inconclusive` when evidence cannot support a
   verdict.
4. Set `confidence_ppm` to the evaluator's confidence that its verdict is
   correct. Use `null` when the evaluator or rubric does not supply confidence.
5. Write a draft subject with an empty `evidence_digest`, then seal it:

   ```bash
   bitrouter eval subject seal subject-draft.json --output subject.json
   ```

6. For a multi-decision subject, derive `decision_credit` from the fixed
   evaluator credit policy:
   - Exact supported decision/metric mappings: emit only those mappings.
   - No policy or no exact mapping: use `{}` or omit the serde-defaulted field.
     The result remains a record but produces no per-route evidence.
   For a one-decision subject, empty credit means implicit full credit. When an
   inconclusive evaluator intentionally withholds attribution, emit that
   decision with `weight_ppm: 0` instead.
   Keep hypothetical or illustrative weights outside submit-ready JSON.

## Adapt a Harbor run

Use the deterministic external adapter. Raw decision rows require the trace
file so the adapter can establish exact content and ingress identity; prejoined
rows may omit `--traces`.

```bash
python3 scripts/terminal_bench_route_evidence.py \
  --run-dir /path/to/harbor-run \
  --decisions /path/to/policy-decisions.jsonl \
  --traces /path/to/traces.jsonl \
  --output-dir /path/to/evolution-analysis
```

Inspect `join-summary.json` before any matrix row. Treat unmatched or ambiguous
joins as inconclusive. `packets.jsonl` is the generic Eval Exchange handoff;
`task-evidence.jsonl`, `matrix.json`, and `matrix.csv` are external analysis.

Keep the two recommendation layers separate:

- Only strict unique task attribution can set `quality_credit_eligible` or an
  `active_recommendation`.
- `controlled_validation_candidate` and `screening_reason` are non-causal
  observational screening. They carry zero Eval quality credit and cannot
  publish or edit a policy. A balanced candidate remains balanced until a
  controlled evaluation satisfies the strict gate.

## Submit and hand off

1. Insert the sealed subject and submit a result that repeats its exact
   `eval_id` and `evidence_digest`.

   ```bash
   bitrouter eval subject put subject.json --config bitrouter.yaml
   bitrouter eval result submit result.json --config bitrouter.yaml
   ```

2. Treat an `admitted` response as eligible evidence. Preserve `held_out`,
   `rejected`, and `disputed` responses as non-training records.
3. Hand the sealed subject, result, submission response, and private evidence
   references to the operator. Stop before snapshot, compile, diff, or publish.

## Keep the packet consistent

- Use `subject seal` for canonical evidence hashing and JSON.
- Attribute metrics only to evidence-supported decisions.
- Never copy a task- or episode-level reward onto each request. Use a fixed
  causal policy (for example, a matched control plus one changed route family)
  or withhold credit.
- Preserve the router-authored baseline and selected tiers.
- Treat `inconclusive` as zero quality evidence even if an old or malformed
  packet assigns positive quality credit. Attribute cost or latency separately.
- Keep evaluator identity, rubric/config digest, evidence references,
  confidence, and idempotency key stable for an equivalent retry.
