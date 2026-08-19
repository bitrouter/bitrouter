# Strong-tier cost-budget optimization final review

## Outcome

- Promoted only `agent_route/v1|unknown|mechanical|guarded` from `balanced`
  to `strong` in the generic auto-router template.
- Left the progress guard unchanged at `[strong, balanced]`.
- Kept Terminal-Bench parsing, strict-cohort filtering, and counterfactual
  costing in the external `evaluating-bitrouter-routes` skill.
- Added no benchmark-specific Rust runtime branch, task identifier, or parser.

## Frozen evidence result

The private evidence bundle is outside the repository at:

`/Users/archer/.codex/worktrees/9907/bitrouter-ws/artifacts/tb21-v1-full1-20260818T075324Z/analysis/strong-tier-optimize-20260819`

The deterministic planner reproduced:

- 22 fully clean tasks;
- 309 exact request/decision joins;
- 22 balanced requests promoted in the counterfactual;
- strong request share from 82/309 (26.54%) to 104/309 (33.66%);
- current nominal cost `$6.324025900`;
- candidate nominal cost `$7.105622196`, or `$0.322982827` per task;
- separate control-attempt savings of 56.2495%, 45.8309%, and 55.0950%;
- user-approved cheapest exact-case anchor: attempt 2 at 45.8309% savings.

This is a token-preserving nominal repricing. It is not a causal reward
estimate, a prediction of strong-model token behavior, or a settled invoice.
Reasoning remains a subset of completion and is not billed twice.

Artifact hashes:

- `report.md`: `4e18f6d58b720c8e6806acdfc9500cf54de35db5742c288eb10fb595a77fad49`
- `route-cells.csv`: `902f8e9d2efcb18a10fd22d2dd8ce31bfb22f805f6b1ee9f949b4c700e2ca887`
- `sha256-manifest.json`: `607660048beba96ce17ced1d84d0674f4a5574161dda53e4ea299bd6e3b23a8e`
- `summary.json`: `7a6c368daf029d2d65c908cc0c484ae298c31ae5fcfff89da9ac847e9c2efaad`

A fresh replay into `/tmp/bitrouter-strong-plan-review-fix.k61UsU`
matched the frozen artifact byte-for-byte.

## Fail-closed contract

The external planner now rejects:

- a missing `error` member instead of treating it as explicit `null`;
- any non-null request error;
- missing, non-finite, or negative nominal cost and token fields;
- missing, duplicate, or incomplete policy decisions;
- a missing target route;
- a target route whose observed static and selected tiers are not both
  exactly `balanced`.

The cheapest control anchor is selected from the original unrounded Decimal
inputs. All three controls remain separate in output.

## Canonical template evidence

- Compiler config digest:
  `sha256:97064a1403ab8e126d739913a3e70036f5d378b51fe3b6d29cb1d7fd322e200d`
- Target route evidence digest:
  `sha256:81da3d3ba3fd318ab16b6f74cc9ff28dfb5b49f157594f44b4d82c0162a3ee85`
- Evidence root:
  `sha256:9375c4b893238f413c8a094e3b6121972240674fcc8063b55e7e85f1844171c6`

A structural before/after audit asserted exactly one route-tier change,
exactly one certificate selected-tier change, an unchanged progress guard,
and zero newly added Terminal-Bench/Harbor strings in the Rust diff.

## Verification

Fresh final checks on the reviewed tree:

- `cargo test --all-features`: PASS, exit 0. The desktop proxy variables were
  explicitly removed because they otherwise redirect localhost WireMock
  traffic; a representative prior failure passed immediately under the clean
  environment.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  PASS, exit 0.
- `cargo fmt --all -- --check`: PASS, exit 0.
- `git diff --check`: PASS, exit 0.
- strong-tier planner tests: 10/10 PASS.
- existing route-evidence adapter tests: 33/33 PASS.
- installed canonical skill validator: PASS.
- `python3 -m py_compile` for the planner and tests: PASS.
- auto-router canonical template tests: 2/2 PASS.
- auto-router config validation: PASS (`valid: true`).
- private artifact replay: byte-identical, exit 0.

## Independent review

The first independent review found three Important fail-closed gaps and one
Minor anchor-precision issue. RED tests reproduced all four. Commit
`793e963a` closed them, and the reviewer then reported no Critical or Important
findings with verdict **ACCEPT**.

## Publication boundary

The benchmark evidence remains private and is not staged, uploaded, or
published. Only the generic template route, its canonical evidence, the
external reusable skill/planner, tests, and development records are part of
the source branch.
