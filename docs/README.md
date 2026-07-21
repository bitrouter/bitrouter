# BitRouter docs

This folder holds **internal development docs** — the CLI reference, the
workspace architecture guide, and design specs. It is *not* published anywhere.

## Contents

- [`CLI.md`](CLI.md) — full command reference, flags, and config resolution.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals.
- `*_SPEC.md` / `*_ACCEPTANCE.md` — design specs and acceptance criteria for
  in-flight work (TUI, spawn/launch, onboarding, detach/reattach).

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
