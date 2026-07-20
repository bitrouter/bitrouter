# BitRouter docs

The product documentation that used to live here now lives in the
**[bitrouter-docs](https://github.com/bitrouter/bitrouter-docs)** repository, under
`content/docs/` — it is authored, reviewed, and published there.

- Edit docs in `bitrouter-docs`, not here.
- The `supported-models` / `supported-providers` tables are generated on the docs
  site from this repo's committed `dist/registry/{models,providers}.json`
  (`scripts/generate-registry-tables.mjs`), so keep the registry catalog current
  here as usual — the tables follow automatically.
- On each release, an agent in `bitrouter-docs` drafts a docs update from the
  changelog for human review.

`superpowers/` remains here: internal design notes, not published docs.
