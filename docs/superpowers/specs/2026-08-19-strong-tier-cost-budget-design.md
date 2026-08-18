# Strong-Tier Cost-Budget Optimization Design

## Objective

Use the fully clean evidence from private run
`tb21-v1-full1-20260818T075324Z` to raise the quality floor modestly while
retaining an estimated 40%–50% nominal API-cost reduction against the
cheapest exact-case control attempt. The selected generic change promotes
`agent_route/v1|unknown|mechanical|guarded` from `balanced` to `strong`.

## Frozen evidence and validity

The input cohort is the 22-task `fully_clean_tasks` set in the local benchmark
artifact. A task belongs to this set only when it has a numeric verifier
reward, no Harbor exception, and no matched physical request error. The
current request join contains 309 successful provider-reported requests for
those tasks, and the frozen BitRouter daemon log contains exactly one policy
decision for every joined trajectory request.

Provider, network, authentication, rate-limit, transport, and other non-task
errors contribute neither quality evidence nor counterfactual token cost. An
incomplete request-to-decision join fails closed instead of producing a plan.

## Cost method

The analysis keeps each successful request's uncached input, cache-read,
cache-write, and completion token counts fixed. Requests whose matched policy
key equals `agent_route/v1|unknown|mechanical|guarded` are repriced with the
frozen nominal strong-tier rates used by the benchmark settlement:

- uncached input: $5.00 per million tokens;
- cache read: $0.50 per million tokens;
- cache write: $6.25 per million tokens;
- completion, including reasoning: $30.00 per million tokens.

Reasoning tokens are a subset of completion tokens and are not billed twice.
The report lists each exact-case control attempt separately and uses the
cheapest attempt as the conservative planning anchor; it never averages the
control attempts.

This is a token-preserving counterfactual, not a causal reward estimate. A live
strong model may produce a different number of tokens.

## Selected policy change

The 22-task cohort currently routes 82/309 requests (26.5%) to `strong`. The
selected cell accounts for 22 balanced requests across 11 tasks. Repricing
those requests as strong yields:

- 104/309 estimated strong requests (33.7%);
- $7.105622 estimated total nominal cost, or $0.322983 per valid task;
- 45.83% estimated savings against the cheapest matched control attempt.

The cell is generic: it combines the workflow role `mechanical` with guarded
risk. It contains no benchmark task identifier, benchmark parser, or
benchmark-specific runtime branch. Existing progress protection remains
`[strong, balanced]`; the classifier, progress guard, provider selection, and
runtime lookup order do not change.

## Architecture

### External evidence adapter

Add a deterministic script to `skills/evaluating-bitrouter-routes/scripts/`
that consumes the frozen validity audit, request join, and daemon decisions.
It validates the exact join, performs token-preserving repricing, and emits a
machine-readable summary, route-cell CSV, human report, and SHA-256 manifest.
Terminal-Bench field knowledge remains entirely in this skill-owned adapter.

### Generic product template

Change only the selected route in the official auto-router policy template.
Refresh its canonical compiler and route evidence digests through the
repository's existing deterministic template test path. Update template prose
to describe the higher quality floor for guarded mechanical work.

No BitRouter runtime source code receives Terminal-Bench-specific behavior.

## Acceptance criteria

1. The external adapter rejects missing, duplicate, errored, or ambiguous
   strict-cohort request/decision joins.
2. The adapter reproduces 22 tasks, 309 requests, 22 promoted requests,
   33.7% estimated strong share, $7.105622 total cost, and 45.83% savings
   against the cheapest matched control attempt.
3. The report preserves all three control attempts separately and labels the
   calculation counterfactual and non-causal.
4. The official template maps only
   `agent_route/v1|unknown|mechanical|guarded` from balanced to strong; the
   other 17 route cells and progress-guard contract remain unchanged.
5. Template compiler/certificate evidence is regenerated canonically.
6. Focused skill, template, config, full test, Clippy, formatting, and diff
   gates pass before push.
