# BitRouter docs

This folder holds **internal development docs** — the CLI reference, the
workspace architecture guide, and design specs. It is *not* published anywhere.

## Contents

- [`CLI.md`](CLI.md) — full command reference, flags, and config resolution.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals.
- `*_SPEC.md` / `*_ACCEPTANCE.md` — design specs and acceptance criteria for
  in-flight work (spawn/launch, onboarding, the MCP `2026-07-28` upgrade,
  skills over MCP, the observability TUI, the ACP TUI, the ACP controller,
  the control plane).
- [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) — authoritative boundary
  for ACP controller topology, harness-owned sessions, endpoint configuration,
  native identity, and session-scoped routing.
- [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md) — **the live one.** Why
  the OTLP renderer ships as `crates/bitrouter-telemetry` while `bitrouter-sdk`
  keeps only the contract it renders (`observe::schema`, `SpanAttributes`).
  Start here; the two documents below are its history. Read its *The arguments
  that are dead* section before reopening anything — the crate-count and
  build-cache cases were measured, withdrawn, and are not what decided this.
- [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md) — **D1 superseded.**
  Recorded why the exporter moved *into* `bitrouter-sdk` behind an `otel`
  feature. That placement was reversed before it reached a release. Still the
  best record of the hard constraint, the feature shape, and which names,
  targets and config keys are load-bearing.
- [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) — proposed splitting that
  module into schema / emission / export tiers. **Phases 0–2 landed and stand**
  (the committed span-schema artifact, the `tracing` bridge kept, the
  OTel-native ingress span); its D1 was withdrawn on measured benefit and then
  reopened on the positioning grounds it had itself reserved. Read its *cloud
  question* section before reopening any of it.
- `*_PLAN.md` — ordered execution plans derived from a spec, with per-task
  completion criteria. [`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md) is written to be
  driven by `/goal`.

## Where product docs live

The **product** documentation that used to live here now lives in the
**[bitrouter-docs](https://github.com/bitrouter/bitrouter-docs)** repository, under
`content/docs/` — it is authored, reviewed, and published there.

- Edit product docs in `bitrouter-docs`, not here.
- The `supported-models` / `supported-providers` tables are generated on the docs
  site from this repo's committed `dist/registry/{models,providers}.json`
  (`scripts/generate-registry-tables.mjs`), so keep the registry catalog current
  here as usual — the tables follow automatically.
- On each release, an agent in `bitrouter-docs` drafts a docs update from the
  changelog for human review.
