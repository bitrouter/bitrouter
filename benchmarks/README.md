# BitRouter Benchmarks

This directory is the lightweight, versioned index for BitRouter benchmark
experiments. Git contains one numbered Markdown report per experiment; complete
raw traces and machine-readable evidence live in the public
[`BitRouterAI/benchmarks`](https://huggingface.co/datasets/BitRouterAI/benchmarks)
dataset on Hugging Face.

## Experiments

| No. | Experiment | Benchmark | Headline |
| --- | --- | --- | --- |
| 001 | [`2026-07-10-tbench-v2.1-codex-gpt55-kimi-k27`](001-2026-07-10-tbench-v2.1-codex-gpt55-kimi-k27.md) | Terminal-Bench v2.1 | r2 reduced imputed cost by 32.8% at a 1.14 pp score difference; r3 reached the highest score, 82.95%, at 8.2% lower cost. |

## Layout

- Reports use a monotonically increasing numeric prefix followed by the stable
  experiment ID: `<number>-<experiment-id>.md`.
- The same experiment ID names the corresponding directory in the Hugging Face
  dataset.
- Reports summarize the hypothesis, frozen protocol, results, limitations, and
  conclusions. Raw messages, tool calls, reasoning data, model usage, configs,
  scripts, and integrity metadata remain in the dataset rather than this source
  repository.

These reports describe routing mechanism studies unless explicitly marked as a
conformant benchmark submission. Check each report's limitations before using a
score for model or leaderboard comparisons.
