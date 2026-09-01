# Copilot Instructions for BitRouter

**This file is deliberately a pointer, not a document.** Read these instead:

- [`AGENTS.md`](../AGENTS.md) — the repository rules for anything editing this
  codebase: the hard prohibitions (`#[allow(…)]`, `unwrap` / `expect` /
  `panic!`, dead code, re-exports out of a public module), the lockstep
  requirements, and the checks to run before submitting. `CLAUDE.md` is a
  symlink to it.
- [`docs/DEVELOPMENT.md`](../docs/DEVELOPMENT.md) — workspace architecture: the
  crate table, the dependency layering and what belongs in which tier, the
  external interfaces, the SDK's pipelines and hook traits, and the feature
  flags.
- [`docs/CLI.md`](../docs/CLI.md) — command reference, flags, config
  resolution, and log targets.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — adding a provider, and the
  conventional-commit format PR titles are validated against.

## Why this file has no content of its own

It used to. It was added in March 2026, two weeks *after* `AGENTS.md` already
existed, as a Copilot-specific second copy of the same material — and because
nothing kept the copy in sync, it rotted almost immediately. By the time anyone
audited it, it described crates that had been deleted (`bitrouter-core`,
`bitrouter-api`, `bitrouter-config`, `bitrouter-accounts`, `bitrouter-a2a`,
`bitrouter-skills`), Warp filters for an HTTP server that is axum, and an
`AppRuntime` / `ServerPlan` runtime path that was never in the tree it shipped
alongside. An agent following it would have been confidently wrong about
almost every fact in it.

So: **do not re-expand this file.** If Copilot needs to know something about
this repository, that fact belongs in one of the documents above, where the
lockstep rules in `AGENTS.md` apply to it and where a stale line has a chance
of being noticed. A pointer cannot go out of date.
