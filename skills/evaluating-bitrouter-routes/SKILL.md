---
name: evaluating-bitrouter-routes
description: Build and submit evidence-bound BitRouter Eval Exchange results for request, episode, or task outcomes. Use when evaluating route decisions with a task-native verifier, human review, private enterprise system, or agentic rubric; selecting scope, sealing redacted evidence, assigning explicit multi-decision credit, and verifying admission.
---

# Evaluate BitRouter Routes

Evaluate outcomes outside BitRouter's serving path. Produce an immutable result
and stop after BitRouter reports its admission status; an operator owns
snapshots, candidate compilation, diffs, and publication.

Read [the Eval Exchange reference](references/eval-exchange.md) before forming
a subject or result. It is the exact current wire and authority contract.

## Build the evaluator packet

1. Select the smallest scope that contains the outcome: `request` for one
   request-local observation, `episode` for a bounded workflow, or `task` for
   one terminal task outcome.
2. Copy every decision's `decision_id`, `policy`, `request_key`,
   `selected_tier`, `baseline_tier`, and `policy_digest` from router-authored
   evidence. Do not derive policy identity from a preset string or invent a
   counterfactual execution.
3. Redact evidence before it leaves its private source. Retain raw messages,
   tool arguments, code, and evaluator output with the evaluator; place only
   safe, content-addressed evidence items in the subject.
4. List only dimensions the evaluator was asked to judge. Leave unsupported
   dimensions absent rather than recording zero. Use `inconclusive` when the
   evidence cannot support a verdict.
5. Write a draft subject with an empty `evidence_digest`, then seal it with
   BitRouter instead of reimplementing canonicalization:

   ```bash
   bitrouter eval subject seal subject-draft.json --output subject.json
   ```

6. For a multi-decision subject, use an evaluator-configured causal-credit
   policy and explicitly name each credited metric in `decision_credit`.
   Leave unsupported decisions uncredited. A single-decision subject may omit
   credit.

## Submit and hand off

1. Insert the sealed subject and submit a result that repeats its exact
   `eval_id` and `evidence_digest`.

   ```bash
   bitrouter eval subject put subject.json --config bitrouter.yaml
   bitrouter eval result submit result.json --config bitrouter.yaml
   ```

2. Treat only an `admitted` submission response as eligible evidence. Preserve
   `held_out`, `rejected`, and `disputed` responses as records, but do not
   represent them as route-learning input.
3. Hand the sealed subject, result, submission response, and private evidence
   references to the operator. Stop here. Do not freeze a snapshot, compile a
   candidate, diff a policy, or publish a policy.

## Guardrails

- Use `bitrouter eval subject seal`; never hand-roll evidence hashing or
  canonical JSON.
- Attribute only evidence-supported metrics and decisions. Never split a
  terminal reward equally by default.
- Keep the router's known baseline tier; do not replace it with the selected
  tier or an evaluator guess.
- Keep evaluator identity, rubric/config digest, evidence references,
  confidence, and idempotency key stable for a retry.
