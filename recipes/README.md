# BitRouter Recipes

Ready-made **policy specs** for common agentic workflows — a starting routing
configuration you can drop into a loop so it routes well before you tune it
yourself.

Each recipe targets a workflow (a harness plus a task type) and ships the
`bitrouter.yaml` that routes its calls, tools, and agents across the
cost / latency / accuracy objectives BitRouter optimizes for — plus the
**measured** result of running that config against a baseline.

Recipes are the source of truth for the recipe gallery on
[bitrouter.ai/recipes](https://bitrouter.ai/recipes). The site reads the built
catalog `dist/recipes/index.json` straight from this repo, so a merged recipe
reaches the site without a docs change.

## Anatomy

One directory per recipe; the directory name **is** the slug.

```
recipes/<slug>/
  recipe.yaml        # editorial metadata + the measured evaluation
  bitrouter.yaml     # the drop-in policy spec — verbatim, copy-pasteable
  policy-lock.yaml   # optional: the sibling policy lock, when the recipe uses one
  README.md          # long-form body — rendered as the recipe's page
  README.zh.md       # optional Chinese body; the site falls back to English
```

Everything derivable from `bitrouter.yaml` is **derived, never restated** in
`recipe.yaml`: the providers it configures, the models it routes to, and the
environment variables it interpolates are all extracted by the builder. A
hand-maintained second copy would only drift.

### `recipe.yaml`

```yaml
slug: claude-code-cost-cut         # must equal the directory name
status: published                  # draft | published — only published ships
title:
  en: Claude Code, half the spend
  zh: Claude Code：成本减半
description:
  en: Routes the read-heavy majority of a Claude Code loop to an open-weights
    model and keeps the frontier model for edits.
  zh: 将 Claude Code 循环中以读取为主的调用路由到开源权重模型，编辑仍走前沿模型。
workflow: coding                   # the task type this recipe routes
harness: [claude-code]             # harnesses it is written for
objectives: [cost]                 # cost | latency | accuracy — what it optimizes
updated_at: 2026-07-25

evaluation:                        # required once `status: published`
  eval: terminal-bench-2.1
  harness: claude-code             # must be one of `harness:` above
  config: max
  measured_by: bitrouter           # `bitrouter`, or a third-party source name
  as_of: 2026-07-25
  runs: 3
  baseline:
    label: claude-opus-5, no routing
    accuracy: 86.5
    cost_per_task: 1.20
    time_per_task: 4.6
  recipe:
    accuracy: 88.0
    cost_per_task: 0.74
    time_per_task: 4.3
```

## Measured numbers, not claims

A recipe exists to make a comparative claim — *cheaper*, *faster*, or *more
accurate than what you run today*. So the catalog stores **measurements, not
claims**: `baseline` and `recipe` each carry the raw metrics, and the delta the
site renders ("38% cheaper") is computed from them at build time. A stored
percentage can drift from the numbers it came from; a computed one cannot.

What the validator enforces:

- **`published` requires an `evaluation`.** A recipe is evaluated before it is
  released. Work in progress lives at `status: draft`, which is validated like
  any other recipe but is **excluded from `dist/recipes/index.json`**, so it
  never reaches the site.
- **Baseline and recipe must report the same metrics.** A delta between metrics
  that were not both measured is not a delta.
- **Provenance is mandatory**, for the same reason it is in
  [`registry/`](../registry/README.md): a raw score means nothing on its own.
  `eval` + `harness` + `config` pin a run so it is reproducible, `runs` says how
  many times it was repeated, `as_of` dates it, and `measured_by` — with a
  required `source_url` when it is not `bitrouter` — keeps a cited third-party
  number from being mistaken for one we ran.

The metrics are `accuracy` (percent of tasks passed, `0..=100`),
`cost_per_task` (USD), and `time_per_task` (minutes) — the same three the
registry records for Terminal-Bench 2.1.

## What the validator checks

```sh
cargo run -p dist-helper -- recipes validate   # source checks
cargo run -p dist-helper -- recipes build      # regenerate dist/recipes
```

Beyond the schema and the evaluation rules above:

- **The shipped `bitrouter.yaml` really is a config BitRouter accepts.** It is
  parsed through the same `bitrouter_sdk::config` loader the daemon uses, with
  **no environment variables set** — so a recipe can never merge in a shape that
  fails on the user's first `bitrouter start`.
- **Every provider it configures exists in the registry**, and every model
  endpoint it routes to is one that provider actually serves. A recipe pointing
  at a withdrawn provider or a model id that moved fails CI here.
- **Translations are advisory, for now.** The gallery renders on the site's
  English-only marketing routes, so a published recipe without `zh`
  title/description — or without `README.zh.md` — reports an advisory rather
  than failing. A *blank* `zh` value is still an error. This hardens to a hard
  requirement the day those routes are localized.

`dist/recipes/` is generated; never hand-edit it. Commit it alongside your
source changes — `cargo run -p dist-helper -- check` fails when it is stale.

## Contributing

Open a PR with the recipe directory and the rebuilt `dist/recipes/`. Land it as
`status: draft` while the numbers are unmeasured, and flip it to `published` in
the PR that adds the `evaluation` block.

Want a recipe for your workflow but don't want to write it yourself? Open an
issue or email [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai).
