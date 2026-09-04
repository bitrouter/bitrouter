# Agent Skills

This directory is the **source of truth** for BitRouter's [Agent Skills](https://agentskills.io).
They live in the monorepo so each skill's facts (port `4356`, env var names, CLI
subcommands, harness wiring, benchmark evidence contracts) stay in lockstep with
the code that defines them — a skill change ships in the same PR as the change
that motivates it.

## What's here

Skills follow the [Agent Skills specification](https://agentskills.io/specification).

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

A CLI-only activation runbook: probe, install, drive `bitrouter init --yes`,
resolve credentials, wire the harness with `bitrouter launch`, verify a routed
request. Deeper CLI surface, cloud onboarding, provider config, migration off
other gateways, diagnostics, and durable per-harness wiring live in `references/`.

### `/run-bitrouter-benchmark`, at [`run-bitrouter-benchmark/`](run-bitrouter-benchmark/)

```
skills/run-bitrouter-benchmark/
├── SKILL.md          # decision and navigation layer — keep under ~200 lines
├── agents/openai.yaml
└── references/       # Harbor, agents, AWS operations, evidence and publishing
```

Runs installed Harbor benchmarks through an existing or AWS-operated BitRouter
endpoint. It selects the benchmark, agent, route config, provider/model targets,
Harbor attempts/concurrency, AWS identity when needed, and publication intent
without replacing Harbor's scheduler or shipping infrastructure code.

### `/evaluating-bitrouter-routes`, at [`evaluating-bitrouter-routes/`](evaluating-bitrouter-routes/)

```
skills/evaluating-bitrouter-routes/
├── SKILL.md          # evaluator workflow and hard stop at admission
└── references/       # Eval Exchange wire contract and examples
```

Builds redacted, evidence-bound request, episode, and task evaluations for the
generic Eval Exchange. It teaches evaluator work only: an operator owns
snapshots, candidate compilation, and publication.

## Install

All three skills are installable directly from this repository; select a
specific skill explicitly because the source exposes more than one `SKILL.md`.

BitRouter does not install skills — it *serves* them. Use the generic skills
CLI, a plugin marketplace, or copy the directory.

```bash
# Generic skills CLI — discovers skills/ automatically. Always pass --skill:
# without it the CLI installs all three (or prompts, on an interactive TTY).
npx skills add bitrouter/bitrouter --skill bitrouter
npx skills add bitrouter/bitrouter --skill run-bitrouter-benchmark
npx skills add bitrouter/bitrouter --skill evaluating-bitrouter-routes
npx skills add bitrouter/bitrouter --list          # see what the repo exposes

# Claude Code / Codex — add this repo as a plugin marketplace, which ships
# skills/ verbatim (see .claude-plugin/ and .agents/plugins/).

# Manual
cp -r skills/bitrouter                    ~/.claude/skills/
cp -r skills/run-bitrouter-benchmark      ~/.claude/skills/
cp -r skills/evaluating-bitrouter-routes  ~/.claude/skills/
```

Once installed, `bitrouter mcp serve --backend skills` serves them to any MCP
client over SEP-2640 (`skills/list`, `skills/get`, `resources/read`), and
`bitrouter skills list` shows what is installed.

## Editing conventions

- Keep each `SKILL.md` under ~200 lines; deep detail goes in `references/`.
- Each reference file is independently consumable — don't assume a sibling was loaded.
- When you change a CLI flag, port, env var, or harness step in the code, update
  the matching fact here in the same change. See the "Facts that are easy to get
  wrong" section in the repo's agent guidance.
- Validate with the [skills-ref](https://github.com/agentskills/agentskills/tree/main/skills-ref) library: `skills-ref validate ./skills/bitrouter`, `skills-ref validate ./skills/run-bitrouter-benchmark`, and `skills-ref validate ./skills/evaluating-bitrouter-routes`.
