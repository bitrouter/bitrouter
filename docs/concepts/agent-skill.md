---
title: Agent Skill
description: The /bitrouter Agent Skill — so AI coding agents can install, configure, migrate to, and troubleshoot BitRouter on their own.
---

The **Agent Skill** is the third way to drive BitRouter, alongside the [CLI](/docs/concepts/cli) and [MCP](/docs/concepts/mcp). BitRouter ships a `/bitrouter` [Agent Skill](https://agentskills.io) so AI coding agents can install, configure, migrate to, and troubleshoot BitRouter **on their own** — the skill carries the facts that drift easily (the listen port, env var names, the `provider/model` slash form, which subcommands exist) so an agent doesn't guess them.

It lives in the main repo at `skills/bitrouter/`, kept in sync with the code in the same change — so the skill never describes a CLI that no longer matches the binary.

## Install it

```bash
bitrouter skills add bitrouter        # via BitRouter's own installer
npx skills add bitrouter/bitrouter    # via the generic skills CLI
```

Either command drops the skill into your agent's skills directory; from then on the agent can drive BitRouter without you hand-writing setup steps.

## Not the AgentSkills gateway

Two BitRouter-and-skills roles, kept distinct:

- **This page — the shipped skill.** `/bitrouter` is a skill that *drives* BitRouter: an agent reads it to operate the router correctly.
- **The [AgentSkills gateway](/docs/concepts/tools).** BitRouter *serves* skills as governed, routable resources to the agents behind it — installing and exposing skills the way the [MCP gateway](/docs/concepts/tools) exposes tools.

One teaches an agent to use BitRouter; the other lets BitRouter hand skills to agents. See [Tools](/docs/concepts/tools) for the gateway.
