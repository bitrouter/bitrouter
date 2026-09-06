---
type: changed
breaking: true
title: "`bitrouter skills list --json` changes shape and sees more skills"
pr: 870
---

`bitrouter skills list` and the MCP `skills_search` tool are now one action over
one report type.

```jsonc
// before
{ "skills": [{ "name": "alpha", "path": "/p/.claude/skills/alpha" }] }

// after
{ "skills": [{
    "name": "alpha",
    "description": "What alpha does",                // new
    "dir": "/p/.claude/skills/alpha",                // was `path`
    "skill_md": "/p/.claude/skills/alpha/SKILL.md",  // new
    "valid": true,                                   // new
    "problem": "…"                                   // new, omitted when valid
}] }
```

`path` meant the skill *directory* to the CLI and the *`SKILL.md` file* to the
MCP tool. Neither was wrong; sharing one key for both was, so both surfaces now
carry `dir` and `skill_md`. Migrate `.path` to `.dir` or `.skill_md` depending
on which one you meant.

Three behaviour fixes ride along, all from collapsing three discovery rules over
two roots into one:

- **All three conventional layouts are listed.** `bitrouter skills list` read
  only `<root>/.claude/skills`, so a `./skills/foo` skill was invisible to it
  while the agent could see it.
- **A skill that cannot be loaded is listed and explained**, with `valid: false`
  and a `problem`, instead of being listed unmarked by the CLI and dropped
  silently by the agent's surface. SEP-2640's `skills/list` still publishes only
  the loadable ones, which the specification requires.
- **The MCP surfaces read the user-global root**, which only `-g` used to reach.
  An agent no longer misses a skill because of where it was installed.
