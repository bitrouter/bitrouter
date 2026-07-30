---
name: evaluating-bitrouter-routes
description: Use when evaluating BitRouter route decisions or Eval Exchange subjects with task-native verifiers, human reviewers, private enterprise evaluators, agentic judges, or genuinely uncategorized evaluator sources.
---

# Evaluate BitRouter Routes

Evaluate outcomes outside BitRouter's serving path. Produce an immutable result
and stop after BitRouter reports its admission status; an operator owns
snapshots, candidate compilation, diffs, and publication.

Read [the Eval Exchange reference](references/eval-exchange.md) before forming
a subject or result. It is the exact current wire and authority contract.

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

6. For a multi-decision subject, apply an evaluator-configured causal-credit
   policy and name each credited metric in `decision_credit`. Leave unsupported
   decisions uncredited. A single-decision subject may omit credit.

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
- Preserve the router-authored baseline and selected tiers.
- Keep evaluator identity, rubric/config digest, evidence references,
  confidence, and idempotency key stable for an equivalent retry.
