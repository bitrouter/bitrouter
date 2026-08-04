# BitRouter Recipes

Recipes are the measured, discoverable publication layer for reusable routing
templates. A template under [`templates/`](../templates/) owns the deployable
`bitrouter.yaml` and `policy-lock.yaml`; a recipe adds editorial metadata,
evaluation provenance, and the limitations needed to interpret its result.

This split keeps one runtime source of truth. The recipe catalog embeds the
exact template artifacts in `dist/recipes/index.json`, so users can still copy a
complete configuration from the gallery without this repository maintaining two
copies that can drift.

The gallery at [bitrouter.ai/recipes](https://bitrouter.ai/recipes) consumes the
committed catalog directly. A merged published recipe reaches the site without a
separate docs change.

## Anatomy

One directory per recipe; the directory name is the slug.

```text
recipes/<slug>/
  recipe.yaml        # template reference, editorial metadata, evaluation
  README.md          # evidence, limitations, and adoption guidance
  README.zh.md       # optional Chinese body

templates/<template>/
  bitrouter.yaml     # deployable configuration; runtime source of truth
  policy-lock.yaml   # deterministic serving policy
  README.md          # runtime and tuning instructions
```

`recipe.yaml` names the template rather than copying its files:

```yaml
slug: auto-router
status: published                 # draft | published
template: auto-router             # templates/auto-router/
title:
  en: Adaptive @auto routing for agent workflows
  zh: 面向 Agent 工作流的自适应 @auto 路由
description:
  en: Uses native request history to route qualified mechanical work.
  zh: 使用原生请求历史路由符合条件的机械性工作。
workflow: agentic
harness: [generic]                # applicability, not a decision key
objectives: [cost, accuracy]      # cost | latency | accuracy
updated_at: 2026-08-04

evaluation:                       # required once status is published
  eval: terminal-bench-2.1-short13
  harness: terminus-2             # evidence provenance, not recipe identity
  config: frozen-auto-r3-paired-lineages
  measured_by: bitrouter
  source_url: https://github.com/bitrouter/bitrouter/pull/768
  as_of: 2026-08-04
  runs: 2                         # accepted independent runs
  artifacts:                      # exact bytes used by the accepted runs
    config_sha256: sha256:...
    policy_lock_sha256: sha256:...
  baseline:
    label: Fixed strong-model control
    accuracy: 80.7692
    cost_per_task: 0.429854
  recipe:
    label: Frozen @auto policy
    accuracy: 84.6154
    cost_per_task: 0.359071653846
```

## Stored measurements, computed claims

The catalog stores baseline and recipe measurements. It computes accuracy
movement in points and cost/time movement in percent at build time. A percentage
cannot disagree with the raw values because contributors never enter one.

Both sides must report the same metrics. A published recipe requires an
evaluation with a date, accepted-run count, evaluator identity, reproducible
source, and SHA-256 digests for the exact config and policy lock. The builder
rejects publication if either template file has changed since measurement.
Third-party measurements require an HTTPS citation. Rejected attempts,
operational failures, pricing basis, and scope limitations belong in the recipe
body; they must not be silently folded into or deleted from accepted effect
measurements.

The current metrics are:

- `accuracy`: percent of tasks passed (`0..=100`);
- `cost_per_task`: USD per task, with the pricing basis explained in the body;
- `time_per_task`: minutes per task.

## Validation

```sh
cargo run -p dist-helper -- recipes validate
cargo run -p dist-helper -- recipes build
```

The validator enforces:

- the named template exists and its `bitrouter.yaml` parses through the same
  config loader used by BitRouter, with no ambient environment values;
- the template's provider and model declarations exist and are active in the
  registry;
- every tier referenced by the current v2 policy lock resolves to a model the
  configured provider or registry serves;
- every preset-bound policy exists in the sibling lock;
- published evaluation digests match the named template byte for byte;
- only `published` recipes reach `dist/`, and publication requires measured
  evidence;
- the evaluation harness records where evidence came from. It does not become a
  route key or force a generic recipe to claim harness-specific logic;
- translations are advisory while the gallery routes remain English-first; a
  present but blank translation is always invalid.

The generated `dist/recipes/` directory must be committed with source changes.
`cargo run -p dist-helper -- check` fails when it is stale.

## Contributing

Add or update the runtime artifact under `templates/` first. Then add a recipe
as `draft` while its result is unmeasured. Publish only when the exact template
has accepted comparative evidence and the body records limitations and rejected
attempts honestly.
