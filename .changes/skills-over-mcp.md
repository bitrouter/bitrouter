---
type: added
title: "Agent Skills are served and proxied over MCP (SEP-2640)"
pr: 770
---

BitRouter now serves Agent Skills as an MCP server and proxies them as a
gateway, over stdio and Streamable HTTP alike. `bitrouter mcp serve --backend
skills` answers `skills/list`, `skills/get`, `resources/list`, and
`resources/read` over the installed-skills root, with complete `sha256:`
digests per file; the existing `skills_search` / `skills_get` tools are
unchanged and still served. The daemon's aggregate `POST /mcp` merges upstream
skill catalogs, namespacing each under its configured server name
(`skill://<server>/<skill-path>/SKILL.md`) so two upstreams publishing the same
URI cannot shadow one another. BitRouter is a skills server and gateway, never
a host: no daemon path installs skills, and gateway-sourced content never
touches a filesystem skill-discovery path.

Caveats are documented in `skills/bitrouter/references/mcp-server.md` — the
gateway is not a security boundary, and remote catalogs are daemon-scoped
rather than caller-scoped.
