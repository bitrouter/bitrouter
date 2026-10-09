# Eval-compiled routing policy

The active version 4 policy lock is the sole source of semantic route intent.
Request and Core session adapters use the same System One assessment and action
planner. Provider routing, context authority and execution constraints still
have to admit the selected action. Evaluation records cannot mutate serving.

## Serving contract

A named router selects a policy. The SDK admits the entry and freezes its policy
snapshot before semantic inference. Classification produces task family, next
role and progress; insufficient evidence produces explicit unknown labels.
The policy matches `semantic_route/v1|<family>|<role>|<risk>`, then the
unknown-family cell, then its default. Tool, progress and continuation constraints
apply within the same snapshot.

Each tier is a complete structured action:

```yaml
economy:
  model: provider/model
  effort: low
  context: evidence
strong:
  model: provider/strong-model
  context: preserve
```

`effort` is optional and caller effort remains an independent constraint.
`context` is required: `preserve` keeps the offered source view; `evidence`
permits only representations offered by the context owner and admitted by its
capabilities. A semantic label cannot authorize omission, recall or extraction.
The physical provider catalog remains authoritative for endpoint selection.

Compiled routes carry classifier cohort commitments. A different backend model,
rubric or confidence threshold cannot silently reuse that route; serving falls
through to another admitted cell or the default. An operator-owned route may
explicitly apply across classifier cohorts. Unassessed evidence is a separate
cohort, not evidence for a later semantic backend.

## Evidence and quality

The existing Eval Exchange owns immutable request, episode and task subjects,
external evaluator identities, results, explicit decision credit and evidence
snapshots. Route measurement schema 2 records model, effort and context strategy
in candidate identity. Action probabilities describe assignment before guards;
classifier distributions describe semantic judgments. Neither is a quality
score.

Request settlement exports bounded semantic receipts and exact selected-plan
receipts. The former commits to input, rubric, backend, threshold and raw
judgments. The latter commits to the canonical prompt and chosen model/view.
Trajectory settlement preserves these receipts through its transactional outbox.
Reused semantic receipts deduplicate by identity; conflicting reuse is rejected.

Transport success and operational completion never imply task success. Built-in
trajectory observations remain inconclusive. External evaluation supplies quality
and explicit credit. The compiler excludes request subjects from independent
quality samples, prefers task attribution over overlapping episode attribution,
and counts a credited decision once per policy/evaluator identity. Optimization
counts each experiment assignment unit once across observations and scopes.
Mixed classifier or evaluator cohorts cannot justify one promotion.

Semantic evaluations require the frozen action candidate catalog. Compilation
checks selected and baseline actions against the current model/effort/context
catalog, preventing evidence for one context treatment from being relabeled as
another. Unknown prices and unknown costs remain unknown.

## Compilation and publication

The lifecycle is:

```text
active lock → classify and route → settle evidence → external evaluation
            → freeze evidence snapshot → compile candidate → publish
```

Compilation is deterministic over the current lock, parent digest, frozen Eval
snapshot, quality criteria and explicit guard proposals. Precedence is guard
constraints, operator ownership, negative Eval evidence, positive Eval evidence,
then inherited certified routes. Conflicts with operator routes are reported.
Changing a guard requires explicit compiler input.

Every explicit route has a certificate containing its owner, selected and
baseline tiers, evidence source, sample counts, quality/economics/latency
summaries where known, verdict, classifier cohort, evaluator configuration,
compiler configuration and evidence digest. The artifact records parent and
snapshot lineage. Raw evidence remains in the ledger, not the YAML.

Candidate compilation does not change the live lock. Publication validates the
candidate and expected parent digest, then replaces the file atomically under a
publication lock. A concurrent operator edit causes publication to fail. Runtime
mode and publication authority remain host-owned.

## Breaking changes

Only `lockfileVersion: 4` is accepted. Versions 1–3, scalar tier targets, the
`fingerprints` alias, global `policy_table`, legacy migration certificates and
legacy adequacy-to-route compilation are removed. There is no implicit conversion
of scorecard routes or database rows into semantic evidence. Start with an
explicit operator policy or the conservative, route-empty starter template,
then collect evidence under the configured classifier.

See [the unified routing design](BRO_DECISION_NATIVE_SPEC.md) and
[the Eval Exchange skill](../skills/evaluating-bitrouter-routes/references/eval-exchange.md)
for the runtime boundary and exchange workflow.
