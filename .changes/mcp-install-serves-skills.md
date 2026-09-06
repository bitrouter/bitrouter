---
type: fixed
title: "A `bitrouter mcp install`-ed client sees the skills tools"
pr: 870
---

`mcp install` writes `["mcp", "serve"]`, but the skills and SEP-2640 surfaces
were wired only under `mcp serve --backend skills`, so an installed client never
saw a skill.

Every **stdio** profile now carries them — the identity argument that makes
`--backend skills` stdio-only (the server is a subprocess of the caller, whose
machine it is) holds identically for stdio `mcp serve`. The multi-tenant HTTP
profile is unchanged and still carries neither.
