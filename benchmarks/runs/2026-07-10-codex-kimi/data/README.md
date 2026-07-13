# Derived evidence

The complete derived evidence for this run, committed directly (≈13 MB of text).
This is everything needed to independently recompute every number in
[`../results.json`](../results.json) and the report table.

The raw HTTP capture (`traces.jsonl`) is **not** included: its request bodies
carried provider account identifiers and auth tokens. These derived files are
produced from those traces with the sensitive transport stripped out, and they
contain no task prompts or agent solutions — only metering, decisions, outcomes,
and policy state.

## Layout

```
control/  r1/  r2/  r3/    one directory per group
support/                   cross-round analyzer output + run manifest
```

Each group directory contains:

| File | What it is |
| --- | --- |
| `benchmark-outcomes.jsonl` | One row per comparable task: `task_id`, `reward` (1.0 pass / 0.0 fail), timing, `session_key`. 88 rows per group. |
| `cloud-usage.jsonl` | Per-request token usage (input/output, strong vs weak), the basis for imputed cost. |
| `policy-decisions.jsonl` | Per-request routing decision: request key/fingerprint, chosen tier, model. (Policy groups only; control makes none.) |
| `run-artifact.json` | Group-level run metadata. |
| `shadow-policy.json` | Shadow-policy evaluation output. |
| `summary.json` | Group summary (counts, totals). |

Policy groups (`r1`–`r3`) also include the policy **learning state** that carries
between rounds: `exploration-state.txt`, `pin-state.txt`,
`semantic-success-state.txt`, `reward-feedback.txt`.

`support/` holds `analyzer-summary.json` (the cross-round analyzer output),
`manifest.tsv` (the run's accepted-trial manifest), and `run.complete`.

## Recompute the headline numbers

The pass counts in `results.json` come straight from `benchmark-outcomes.jsonl`:

```sh
for g in control r1 r2 r3; do
  n=$(wc -l < "$g/benchmark-outcomes.jsonl")
  p=$(grep -c '"reward":1.0' "$g/benchmark-outcomes.jsonl")
  echo "$g: $p / $n"
done
# control: 68 / 88   r1: 70 / 88   r2: 67 / 88   r3: 73 / 88
```
