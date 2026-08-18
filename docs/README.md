# BitRouter docs

This folder holds **internal development docs** — the CLI reference, the
workspace architecture guide, and design specs. It is *not* published anywhere.

## Contents

- [`CLI.md`](CLI.md) — full command reference, flags, and config resolution.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals.
- `*_SPEC.md` / `*_ACCEPTANCE.md` — design specs and acceptance criteria for
  in-flight work (spawn/launch, onboarding, the MCP `2026-07-28` upgrade,
  skills over MCP, the observability TUI, the ACP TUI).
- [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md) — why the OTLP
  exporter moved into `bitrouter-sdk` behind the `otel` feature, and which
  names, gates, and signatures are load-bearing as a result.
- [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) — proposed splitting that
  module into schema / emission / export tiers. **Decided: the schema half
  shipped, the dependency move is withdrawn.** Phases 0 (the committed
  span-schema artifact) and 1 (the `tracing` bridge stays) have landed; phase 2
  (an OTel-native ingress span) is the remaining work; the tier relocation was
  withdrawn on measured benefit, not on feasibility. Read its *cloud question*
  section before reopening any of it.
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
