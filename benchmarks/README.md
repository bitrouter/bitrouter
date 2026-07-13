# BitRouter Benchmarks

Public, reproducible benchmark evidence for BitRouter's routing behavior. Every
run is kept here permanently — this folder is **append-only**. We never overwrite
or delete a past run, so the numbers can always be re-checked and successive
runs can be compared.

Each run is self-contained in git: the report, machine-readable results, frozen
config, and the derived evidence needed to recompute every number. The raw HTTP
traces are **not** published — their request bodies carried provider account
identifiers and auth tokens — but the derived metering, decisions, outcomes, and
policy state that reproduce the results are committed under each run's
[`data/`](runs/2026-07-10-codex-kimi/data/) (≈13 MB of text, no task prompts or
solutions).

## Runs

| Date | Run | Benchmark | Headline |
| --- | --- | --- | --- |
| 2026-07-10 | [`codex-kimi`](runs/2026-07-10-codex-kimi/) | Terminal-Bench 2.1 | Adaptive routing (r2) cut cost **32.8%** vs a strong-only control at ~parity score (−1.1 pp); best round (r3) scored **82.95%** at −8.2% cost. |

**Latest:** [`runs/2026-07-10-codex-kimi`](runs/2026-07-10-codex-kimi/)

## What each run contains

```
runs/<date>-<name>/
  report.md          Full experiment write-up: goal, protocol, results, findings
  results.json       Machine-readable per-group metrics + lifecycle summary
  manifest.json      Run id, BitRouter commit, protocol params, exclusions, conformance
  config/            Frozen experiment definition: Harbor/BitRouter YAML + comparable-task set
  data/              Derived evidence per group (metering, decisions, outcomes, policy state) + README
```

## Protocol and limitations

**Read this before citing any number.** The current run is a **mechanism study**,
not a conformant leaderboard submission:

- **One attempt per task.** Single-attempt scores cannot resolve small
  differences; treat ±1 task as noise. The official Terminal-Bench 2.1
  leaderboard requires ≥5 trials per task.
- **`timeout_multiplier` = 1.5** and custom sandbox sizing. The official
  leaderboard requires the multiplier unset or 1.0 and no resource overrides.
- **Same-task reward supervision.** The r1→r3 lineage learns on the same tasks it
  is scored on. This measures *mechanism convergence* — whether later rounds
  replace strong-model calls with cheaper calls while holding score — **not
  held-out generalization**. A separate tuning/held-out protocol is future work.
- **Cost is normalized, API-equivalent imputed cost** — computed from measured
  token usage at the list per-token prices in `results.json`, so the comparison
  across routes is reproducible at published prices. It is a modeled
  routing-economics figure, not a billing statement.

Because of the first two points, this run **is not** and does not claim to be a
Terminal-Bench 2.1 leaderboard entry. A conformant submission is tracked
separately (see "Roadmap").

## Verifying the numbers

The values in `report.md` / `results.json` are derived from the evidence
committed under each run's `data/`. To audit them yourself, clone the repo and:

1. Read `data/README.md` for the file layout.
2. Recompute the pass counts directly from `data/<group>/benchmark-outcomes.jsonl`
   (there's a one-line snippet in `data/README.md`), and the token totals from
   `data/<group>/cloud-usage.jsonl`.

No download or checksum step is needed — the evidence travels with the clone.

## Reproducing a run

The `config/` directory holds the frozen experiment definition for that run — the
Harbor and BitRouter YAML configs and the comparable-task set
(`comparable-tasks.json`) that define it. Infrastructure-specific values (internal
IPs, SSH key paths, the strong-route provider endpoint) are placeholdered — see
[`config/REDACTIONS.md`](runs/2026-07-10-codex-kimi/config/REDACTIONS.md); secrets
were never committed. The orchestration scripts are infra-specific and non-runnable
outside the original environment, so they are not published; the recovery and
acceptance protocol they implemented is documented in `report.md`. Pin BitRouter
to the `commit` in `manifest.json` to reproduce.

## Roadmap

- **Held-out generalization run** under a tuning/held-out split, to distinguish
  policy effect from single-attempt variance.
- **Conformant Terminal-Bench 2.1 submission** (≥5 trials, multiplier 1.0, no
  overrides) as a third-party-validated capability + cost anchor, pending a
  maintainer decision on how a routing layer populates the leaderboard's single
  `model_name` field. When accepted, the submitted artifact will be mirrored
  under `official/`.
