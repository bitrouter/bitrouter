# Change files

One Markdown file per pull request, describing what the change means for
someone upgrading. Files land here on the feature branch and are folded into
`CHANGELOG.md` — then deleted — when release-plz cuts the release.

This replaces writing prose under `## [Unreleased]` in `CHANGELOG.md`: two
agents working in parallel conflict on a single shared section, and never on
separate files.

## Why not just the commit log

The changelog's primary reader is the release agent in `bitrouter-docs`, which
drafts the docs update from it. `fix(responses): bind completed continuation`
tells that agent nothing. A change file does, because it is written on the
branch, by whoever (or whatever) made the change, while the reason is still
known.

release-plz still derives the **version** from conventional commits, so commit
discipline still matters. It just no longer decides what the changelog *says*.

## Format

`.changes/<short-kebab-slug>.md`:

```markdown
---
type: removed
breaking: true
title: "The `bitrouter skills` installer subcommands are gone"
pr: 770
---

`bitrouter skills add`, `remove`, `find`, and `update` are removed. Install
skills with `npx skills add` or a plugin marketplace instead; BitRouter reads
the installed-skills directory and serves it over MCP.
```

Front matter:

| key        | required | value                                                                        |
| ---------- | -------- | ---------------------------------------------------------------------------- |
| `type`     | yes      | `added`, `changed`, `deprecated`, `removed`, `fixed`, `security`              |
| `title`    | yes      | one line, double-quoted, written for a reader upgrading — not a commit subject |
| `breaking` | no       | `true` renders it under **Breaking changes** instead of its own type section  |
| `pr`       | no       | pull request number; linked from the rendered heading                        |

**Always double-quote `title`.** Titles carry `code spans` and colons, and YAML
reads a leading backtick or an embedded `: ` in a bare scalar as syntax. Quoting
unconditionally is one rule instead of two exceptions.

Unknown keys are rejected — an unrecognised key is a typo, not an extension
point.

The body is prose and carries the actual value. For anything breaking, it must
say **how to migrate**, concretely: the old spelling, the new spelling, and
what happens to existing data or config. Code fences are welcome.

Bodies end up inside `CHANGELOG.md` at the repository root, so write any
relative link from *there* — `[.changes/README.md](.changes/README.md)`, not
`../.changes/README.md`.

## Rules

1. Every PR that changes behaviour a user or an SDK caller can observe adds a
   change file. Pure refactors, test-only changes, and CI plumbing do not —
   label those `no-changelog` and CI will stop asking.
2. One file per distinct user-visible change, not one per PR. A PR that removes
   a CLI surface *and* adds an MCP one writes two files.
3. Name the file after the change, not the branch or the ticket
   (`skills-over-mcp.md`, not `pr-770.md`).

## Commands

```bash
cargo run -p dist-helper -- changelog check
```

Validates every pending file. Also runs as part of `dist-helper check` in CI.

```bash
cargo run -p dist-helper -- changelog fold
```

Folds pending files into the newest `CHANGELOG.md` release section and deletes
them. CI runs this inside the release PR — you do not run it by hand.
