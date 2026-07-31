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

Covers the Local-or-Cloud decision, install, daemon lifecycle, cloud onboarding,
provider config, migration off other gateways, diagnostics, and per-harness wiring.

### `/run-bitrouter-benchmark`, at [`run-bitrouter-benchmark/`](run-bitrouter-benchmark/)

```
skills/run-bitrouter-benchmark/
├── SKILL.md          # decision and navigation layer — keep under ~200 lines
└── references/       # methodology, configuration, operations, acceptance, Q&A
```

Runs or audits reproducible BitRouter Terminal-Bench 2.1 experiments with
Harbor, Terminus 2, a centralized EC2 daemon, and ephemeral EC2 sandboxes. It
keeps AWS identity, models, provider secrets, source revisions, task/trial scale,
and prices configurable while fixing the experimental and evidence method.

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

All three skills are installable directly from this repository; select the benchmark
skill explicitly because the source now exposes more than one `SKILL.md`.

```bash
# BitRouter's own installer (subdir-aware via the registry hub)
bitrouter skills add bitrouter

# Generic skills CLI — discovers skills/ automatically
npx skills add bitrouter/bitrouter

# Claude Code (manual)
cp -r skills/bitrouter ~/.claude/skills/

# Reproducible Terminal-Bench runner skill (BitRouter installer)
bitrouter skills add bitrouter/bitrouter --skill run-bitrouter-benchmark

# Reproducible Terminal-Bench runner skill (generic skills CLI)
npx skills add bitrouter/bitrouter --skill run-bitrouter-benchmark

# Reproducible Terminal-Bench runner skill (Claude Code manual)
cp -r skills/run-bitrouter-benchmark ~/.claude/skills/

# Generic Eval Exchange evaluator skill (BitRouter installer)
bitrouter skills add bitrouter/bitrouter --skill evaluating-bitrouter-routes

# Generic Eval Exchange evaluator skill (generic skills CLI)
npx skills add bitrouter/bitrouter --skill evaluating-bitrouter-routes

# Generic Eval Exchange evaluator skill (Claude Code manual)
cp -r skills/evaluating-bitrouter-routes ~/.claude/skills/
```

## Editing conventions

- Keep each `SKILL.md` under ~200 lines; deep detail goes in `references/`.
- Each reference file is independently consumable — don't assume a sibling was loaded.
- When you change a CLI flag, port, env var, or harness step in the code, update
  the matching fact here in the same change. See the "Facts that are easy to get
  wrong" section in the repo's agent guidance.
- Validate with the [skills-ref](https://github.com/agentskills/agentskills/tree/main/skills-ref) library: `skills-ref validate ./skills/bitrouter`, `skills-ref validate ./skills/run-bitrouter-benchmark`, and `skills-ref validate ./skills/evaluating-bitrouter-routes`.
