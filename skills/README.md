# Agent Skills

This directory is the **source of truth** for BitRouter's [Agent Skill](https://agentskills.io).
It lives in the monorepo so the skill's facts (port `4356`, env var names, CLI
subcommands, harness wiring) stay in lockstep with the code that defines them —
a skill change ships in the same PR as the change that motivates it.

## What's here

One skill, following the [Agent Skills specification](https://agentskills.io/specification).

### `/bitrouter`, at [`bitrouter/`](bitrouter/)

```
skills/bitrouter/
├── SKILL.md          # entry point — keep under ~200 lines
└── references/       # loaded on demand
    ├── cloud-setup.md
    ├── cli.md
    ├── providers.md
    ├── diagnose.md
    ├── migrate-from-*.md
    └── harness-*.md
```

Covers the Local-or-Cloud decision, install, daemon lifecycle, cloud onboarding,
provider config, migration off other gateways, diagnostics, and per-harness wiring.

## Install

Every install rail resolves this directory by its repo path —
`bitrouter/bitrouter` → `skills/bitrouter` — so no separate package is needed.

```bash
# BitRouter's own installer (subdir-aware via the registry hub)
bitrouter skills add bitrouter

# Generic skills CLI — discovers skills/ automatically
npx skills add bitrouter/bitrouter

# Claude Code (manual)
cp -r skills/bitrouter ~/.claude/skills/
```

## Editing conventions

- Keep `SKILL.md` under ~200 lines; deep detail goes in `references/`.
- Each reference file is independently consumable — don't assume a sibling was loaded.
- When you change a CLI flag, port, env var, or harness step in the code, update
  the matching fact here in the same change. See the "Facts that are easy to get
  wrong" section in the repo's agent guidance.
- Validate with the [skills-ref](https://github.com/agentskills/agentskills/tree/main/skills-ref) library: `skills-ref validate ./skills/bitrouter`.
