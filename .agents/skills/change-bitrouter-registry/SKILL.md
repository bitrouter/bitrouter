---
name: change-bitrouter-registry
description: Add, remove, or modify BitRouter model, provider, agent, or runtime registry data and regenerate the committed distribution catalog. Use for registry catalog work; do not use for unrelated provider runtime implementation.
---

# Change the BitRouter registry

Start with `registry/README.md` and the nearest existing YAML entry. Treat the
schema and generated catalog checks as the authority for data shape.

## Classify the change

- For catalog-only changes, edit `registry/` and do not add Rust tests that
  freeze particular entries, prices, provider lists, or catalog counts.
- If the change requires a new authentication handler, transport, protocol, or
  merge behavior, it is also a source change. Inspect the owning crate and add
  behavioral tests for that implementation rather than catalog snapshots.
- Use `registry sync --write` only when the selected catalog feed is meant to
  supply model data. Review its diff; do not treat remote feed output as an
  automatically accepted change.

## Keep consumers aligned

- Rebuild and commit `dist/registry/`; the CLI, packaged distribution, README
  charts, and public-doc generation consume that catalog.
- Update `skills/bitrouter/` when provider setup, environment variables,
  authentication, model spelling, or routing instructions changed.
- Do not hand-maintain model/provider tables in this repository. If prose in
  `bitrouter-docs` hardcodes a changed registry fact and that repository is not
  in scope, report the exact follow-up.

## Verify

For registry data, run in order:

```sh
cargo run -p dist-helper -- registry validate
cargo run -p dist-helper -- registry build
cargo run -p dist-helper -- check
```

Then inspect both the authored YAML and generated JSON diffs. If Rust source
also changed, run the source-code checks required by `AGENTS.md`.
