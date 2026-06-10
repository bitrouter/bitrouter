---
name: cost-routed-subagents
description: >
  Use this skill when a capable "controller" coding session (e.g. Claude
  Code on a flagship model, billed directly to Anthropic) should keep
  orchestrating and reviewing, but hand the bulk of the actual work to
  CHEAPER models to cut cost. Workers run as headless `claude -p` child
  processes whose ANTHROPIC_* environment is pointed at a BitRouter
  Anthropic-compatible endpoint (local daemon at http://127.0.0.1:4356 or
  a managed endpoint), so each sub-task can run on an inexpensive
  open-source model (provider/model ids resolved from /v1/models). Covers
  the cheap-vs-flagship tier decision, the dispatch + two-stage review
  protocol, secret-safe API-key handling (the controller never sees the
  key value), and git-worktree isolation. Trigger on "run subagents on a
  cheaper model", "use bitrouter to make agents cheaper", "delegate this
  to a cheap model and review the result", "cost-routed agents",
  "orchestrate with cheap workers".
version: 1.0.0
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags: [agents, subagents, cost-optimization, routing, anthropic, claude-code, bitrouter, orchestration]
---

# Cost-Routed Subagents

Keep the expensive flagship model for **orchestration and judgment**; push the
**bulk token spend** down to cheap models. The controller (this session) plans,
curates context, dispatches focused workers, and reviews their diffs. Each worker
is a headless `claude -p` process whose `ANTHROPIC_*` environment is redirected at
a **BitRouter** Anthropic-compatible endpoint, so it runs on an inexpensive
`provider/model` while the controller stays on its own (direct) billing.

This is a usage pattern for BitRouter's Anthropic-shaped `/v1/messages` surface.
It uses **only the HTTP API + environment variables** — no BitRouter CLI calls in
the dispatch path, and no custom secret store.

## Why native subagents are not enough

Claude Code's native subagents (the Task tool / `.claude/agents/`) share the
session's `ANTHROPIC_BASE_URL` and credentials — they can pick a model *alias*
but cannot be pointed at a different provider. To route a sub-task to a cheaper
provider you must spawn a **separate process** with its own environment. That is
exactly what this skill does.

## The mechanism (one block)

A worker is `claude -p` with four environment overrides plus `--bare`. The
controller never embeds the key value — it references the variable, the shell
expands it inside the child, so the secret never enters this session's transcript:

```bash
ANTHROPIC_BASE_URL="$BITROUTER_BASE_URL" \   # BitRouter endpoint, Anthropic shape (NO trailing /v1)
ANTHROPIC_AUTH_TOKEN="$BITROUTER_API_KEY" \  # brk_* key — referenced, never printed
ANTHROPIC_MODEL="opencode-go/glm-5.1" \      # a provider/model id from the /v1/models list
CLAUDE_CONFIG_DIR="$HOME/.config/cost-routed-child" \  # clean config home for the worker
claude -p "$(cat task.md)" \
  --bare \                                   # skip hooks/plugins/CLAUDE.md/keychain — a lean worker
  --output-format stream-json \
  --permission-mode acceptEdits \
  --allowed-tools "Read,Edit,Bash,Grep,Glob" \
  --append-system-prompt "$(cat role-prompts/implementer.md)"
```

`./dispatch.sh` wraps exactly this (tier resolution, key preflight, lean config,
redacted `--dry-run`). Prefer it over hand-writing the command:

```bash
./dispatch.sh --tier cheap --task task.md --dir "$PWD" --role implementer
./dispatch.sh --model anthropic/claude-haiku-4-5 --task task.md --dry-run   # prints wiring, key redacted
```

> **Base-URL footgun.** Claude Code speaks the Anthropic Messages API and appends
> `/v1/messages` itself. So `BITROUTER_BASE_URL` for the Anthropic shape **omits**
> `/v1` (e.g. `http://127.0.0.1:4356`). The OpenAI shape *keeps* `/v1`; do not copy
> that here. See [Anthropic Messages API](https://docs.anthropic.com/en/api/messages)
> and [Claude Code settings / env vars](https://code.claude.com/docs/en/settings).

## When to use / when not

**Use when** a task decomposes into focused, well-specified sub-tasks (implement a
spec'd function, fix an isolated test, mechanical refactor, draft docs) that a
cheaper model can finish from fully-provided context, and the controller will
review the result.

**Don't use when** the work is exploratory ("figure out what's broken"), needs the
controller's full conversation context, requires cross-task shared state, or is so
small that a single inline edit is faster than spawning a process.

## Choosing a tier

Match task difficulty to the **cheapest model that can do it**; the controller is
the safety net. See [references/model-tiers.md](references/model-tiers.md) for the
full policy and how to populate tiers from `/v1/models`.

| Tier | For | Resolved from |
|---|---|---|
| `cheap` | mechanical, 1–2 files, clear spec, light tool-use | `$BITROUTER_MODEL_CHEAP` |
| `standard` | multi-file integration, some judgment, heavier tool-use | `$BITROUTER_MODEL_STANDARD` |
| `flagship` | architecture, final review, anything the cheap tiers fail | `$BITROUTER_MODEL_FLAGSHIP` |

Open-source models vary in agentic tool-use reliability. Bias tool-heavy work
toward the stronger tiers, and always let the controller verify.

## Dispatch protocol

The controller curates the worker's full context (workers do **not** read the plan
or this conversation), dispatches, then runs a two-stage review. Workers report a
status: `DONE` / `DONE_WITH_CONCERNS` / `BLOCKED` / `NEEDS_CONTEXT`. Because a
worker is one-shot (no mid-task questions), a `NEEDS_CONTEXT` reply means
re-dispatch with more context — see
[references/dispatch-protocol.md](references/dispatch-protocol.md).

- **Implementer** → `cheap`/`standard` tier (the bulk of token spend).
- **Spec review + code-quality review** → keep on a strong tier; cheap reviewers
  miss things.
- **Final integration review** → the controller (this session), reading diffs.

## Safety

- **Isolate writes.** Run workers inside a dedicated git worktree, not the
  controller's checkout. Constrain `--allowed-tools` and pass only the needed
  `--add-dir`. The controller reviews every diff before integrating.
- **Autonomy.** `--permission-mode acceptEdits` lets a worker edit without
  prompting — combined with a worktree and a tool allow-list, the blast radius is
  bounded.
- **Secrets.** Never `echo`/`printenv`/`cat` the key; the controller only ever
  references `$BITROUTER_API_KEY`. See [references/setup.md](references/setup.md).

## References

| File | When to read |
|---|---|
| [references/setup.md](references/setup.md) | One-time setup: env contract, secret-safe key handling (plain env / direnv / 1Password), the lean child config dir, preflight |
| [references/model-tiers.md](references/model-tiers.md) | Tier policy, populating tiers from `/v1/models`, tool-use reliability notes |
| [references/dispatch-protocol.md](references/dispatch-protocol.md) | Controller loop, status protocol, two-stage review, `NEEDS_CONTEXT` re-dispatch |
| [references/attribution.md](references/attribution.md) | Methodology attribution (adapted from obra/superpowers, MIT) |

The role system prompts live in [`role-prompts/`](role-prompts/) and are injected
into workers via `--append-system-prompt`.

## Relation to the `bitrouter` skill

This skill assumes you already have a reachable BitRouter endpoint. For installing
the daemon, cloud onboarding, minting `brk_*` keys, or wiring a *single* Claude
Code session at `127.0.0.1:4356`, use the `bitrouter` skill's
`references/harness-claude-code.md`. This skill is specifically about **dispatching
cheaper workers from a flagship controller**.
