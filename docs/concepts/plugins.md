---
title: Plugins
description: A plugin packages one or more hooks (plus any migrations) and installs them into a BitRouter pipeline in a single call.
sourceHash: d88f0e88401dc5a409303c6a1597c27c1b52a1c6626eb3d76cefea9ba19d5295
---

A **plugin** is the unit that packages one or more [hooks](/docs/concepts/hooks) — plus any database migrations they need — and installs them into the router in a single call. It's how a capability like guardrails, observability, or attestation ships as one installable piece instead of a loose set of callbacks.

## Pipelines

Every plugin targets one of three pipelines, depending on what kind of traffic it acts on:

- **`language_model`** — the main LLM pipeline, with the full set of hooks.
- **`mcp`** — Model Context Protocol routing.
- **`acp`** — Agent Client Protocol routing.

## A convenience, not the atomic unit

A plugin is a **convenience package**: it bundles a related set of hooks and migrations and installs them together. It is *not* the atomic unit — every plugin can be reproduced by registering its hooks one by one. Bundling them just makes a capability reproducible and installable in one step. The core ships several this way, including `bitrouter-guardrails`, `bitrouter-observe`, and `bitrouter-attestation`.

To write one against the Rust SDK, see [Build a plugin](/docs/guides/build-a-plugin).
