# Routing templates

These templates use the public model-only Policy source contract. A policy
selects a model and optional effort while preserving admitted context.

- [`coding-router`](./coding-router/) initializes `bitrouter/coding` with models
  chosen by the operator. Initialization creates `policy.yaml`.
- [`auto-router`](./auto-router/) supplies an explicit `policy.yaml` with the
  same three tiers and strong default as the bundled `bitrouter/auto` source.

Run each template's README commands from its copied directory. Explicit source
updates require a separate candidate and an expected source digest. Evaluation
facts and source drafts do not activate changes.
