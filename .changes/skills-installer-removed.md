---
type: removed
breaking: true
title: "The `bitrouter skills` installer subcommands are removed"
pr: 770
---

`bitrouter skills add`, `remove`, `find`, and `update` are removed, along with
the `bitrouter-skills` crate that backed them. Installing skills is the
ecosystem's job — `npx skills add`, or the Claude Code / Codex plugin
marketplaces (this repo ships as one). BitRouter **reads** the
installed-skills directory and serves it over MCP; it does not populate it.
That is the same line as "server, not host" applied to content lifecycle:
BitRouter handles transport, not distribution.

`bitrouter skills list` and `bitrouter skills init` remain. `SKILL.md` format
support moved into the binary (`apps/bitrouter/src/skills/`), where its only
consumers live; the git-clone, source-resolution, install-to-disk, and
registry-client code is gone. The `--registry` / `--namespace` flags and the
`api.bitrouter.ai` skills-hub client went with them.
