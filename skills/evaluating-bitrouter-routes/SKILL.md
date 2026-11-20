---
name: evaluating-bitrouter-routes
description: Use when evaluating BitRouter route decisions or Eval Exchange subjects with external verifiers, human reviewers, enterprise evaluators, agentic judges, or generic sources.
---

# Evaluate BitRouter Routes

Evaluate actual outcomes outside the serving path. Read the
[Eval Exchange reference](references/eval-exchange.md) before forming a packet;
the [Eval specification](../../docs/specs/eval/README.md) defines the payloads.

## Bind the subject

Set `spec_version: "eval/1.0.0-beta.1"` on subjects and results. A frozen
snapshot carries that version too. Choose one closed subject identity profile:

- `tenant_model_policy`: exact installation, tenant, configuration revision,
  full Policy ResourceRef and literal selected name. Use revision-bound inference
  and preserve any actual execution/receipt link.
- `tenant_generic`: tenant evidence without Policy identity.
- `gateway_generic`: gateway evidence using the gateway's independent authority,
  without tenant or Policy fields.

Use the matching endpoint. Generic facts cannot justify another Policy revision.

Choose scope from the outcome actually observed: one request is `request`, a
bounded workflow/conversation is `episode`, and an externally defined task with
terminal task/verifier evidence is `task`. Keep logical task identity stable
inside its run/source namespace; give separate attempts distinct evaluation IDs.

## Build the packet

1. Copy router-authored decision IDs, selected/baseline tiers and efforts,
   request/projection keys, diagnostic digests and route measurements exactly.
   Use the exact Policy `RouteClass` strings for `request_key` and
   `route_projection`.
2. Keep raw prompts, code, tool arguments and evaluator output private. Submit
   only redacted, content-addressed evidence and safe attributes.
3. Request only supported dimensions. Keep cost/latency/reliability separate from
   task quality. Use `inconclusive` when evidence does not justify a verdict.
4. Write a draft with an empty evidence digest and seal it locally:

   ```bash
   bro eval subject seal subject-draft.json --output subject.json
   ```

5. Repeat the subject's exact ID and sealed evidence digest on its result.
6. Follow the endpoint's authority rules. Gateway evaluator kind must match its
   configured source/authority. Tenant manual results require `generic`, the
   authenticated principal's authority ID, and no hard violations.
7. Use a fixed evidence-supported decision-credit policy. For multiple decisions
   without an exact mapping, withhold credit. Empty credit on a single decision
   means full credit; explicit zero weight withholds it.

`confidence_ppm` is confidence in the verdict, not a quality score. Use `null`
when the rubric provides none. Report complete task/episode cost at that scope;
never duplicate a terminal reward across its individual requests.

## Submit and hand off

Insert the sealed subject and submit its result using the matching SDK/HTTP
profile. The gateway ledger also has local CLI operations:

```bash
bro eval subject put subject.json --config bitrouter.yaml
bro eval result submit result.json --config bitrouter.yaml
```

Preserve the packet, response, evidence references and actual admission status:
`admitted`, `held_out`, `rejected`, or `disputed`. Do not reinterpret a failed or
interrupted mutation as proof that nothing committed. Keep equivalent retry
identity, rubric/config digest and idempotency key stable.

Stop after the authorized evaluator handoff. Freeze or Policy updates require
those actions to be part of the task. If requested, the client may freeze facts,
independently author a public model-only Policy source, create new immutable
resource/configuration revisions, and explicitly activate with current authority
and CAS. Submission, freeze and drafts never change active Policy automatically.
