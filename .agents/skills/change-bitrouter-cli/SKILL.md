---
name: change-bitrouter-cli
description: Change or review BitRouter CLI commands, flags, defaults, config resolution, output contracts, or interactive commands while keeping the shippable agent skill and plugin entry points aligned. Do not use merely to run BitRouter as an end user.
---

# Change the BitRouter CLI

Treat the implemented CLI and its tests as executable truth. Do not create or
restore a hand-maintained CLI reference under `docs/`.

## Orient

1. Locate the repository root and inspect the relevant Clap definitions under
   `apps/bitrouter/src/`; the root command tree starts in `main.rs` and some
   command families have their own `cli.rs`.
2. Inspect the command handler, typed report or stream contract, and existing
   tests before editing. A flag declaration alone is not the behavior.
3. Read `docs/architecture/overview.md` only if the change affects an external
   interface, crate boundary, or config-resolution ownership.
4. Read the relevant section of `skills/bitrouter/SKILL.md` and
   `skills/bitrouter/references/cli.md` before changing a public surface.

## Implement

- Preserve JSON, human, text, quiet, or NDJSON output contracts that the
  command already exposes unless the requested change explicitly revises one.
- Keep command behavior in shared actions or application services when another
  interface consumes the same operation. Do not duplicate business logic in a
  presentation layer.
- Update `skills/bitrouter/` in the same change for any changed command, flag,
  listen port, environment variable, default config, or harness wiring step.
- If `mcp serve` or another plugin entry point changes, verify
  `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json`
  against the implemented command.
- User-facing explanations belong in `bitrouter-docs`, not this repository's
  `docs/`. When that repository is outside the task scope, report the exact
  public-doc follow-up instead of adding a local duplicate.

## Verify

Run the narrow command tests and inspect the affected `--help` output. If Rust
source changed, finish with the repository-required test, Clippy, and formatting
checks from `AGENTS.md`. Confirm with `git diff` that the public skill and any
affected plugin manifest changed in lockstep.
