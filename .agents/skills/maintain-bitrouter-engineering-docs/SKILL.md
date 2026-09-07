---
name: maintain-bitrouter-engineering-docs
description: Create, reorganize, review, or retire BitRouter's internal architecture documents, invariants, decisions, specifications, and execution plans. Use for repository engineering knowledge; do not use for public product documentation.
---

# Maintain BitRouter engineering knowledge

Read `docs/README.md` before changing the engineering-document tree. It is the
canonical inclusion, placement, and retirement policy.

## Decide whether prose is necessary

1. Prefer code, types, schemas, generated artifacts, or tests for facts that can
   be enforced mechanically.
2. Put short rules that every change must obey in `AGENTS.md`.
3. Put repeatable procedures in a focused `.agents/skills/` workflow.
4. Put detail used only by one workflow in that skill's `references/`.
5. Add an engineering document only for shared architecture, subtle invariants,
   durable rationale, or active multi-phase work.

Do not symlink content between `docs/` and `.agents/skills/`. A development
skill may name the exact `docs/` path to load when shared context is relevant.

## Place and maintain the document

- `docs/architecture/`: describe the current system, not its implementation
  chronology.
- `docs/invariants/`: state the constraint, failure mode, and executable guard.
- `docs/decisions/`: record the decision, alternatives rejected, consequences,
  and reconsideration trigger.
- `docs/work/`: keep active specs, plans, and progress only. Use a clear status
  header and remove the document when the work finishes.

User tutorials, CLI reference, onboarding, and operational guides belong in
`bitrouter-docs`. Shippable user-agent workflows belong in `skills/` and must
remain self-contained.

## Retire work

When work completes, move only still-useful system facts into architecture,
invariants, decisions, code, or tests. Delete the completed plan and superseded
spec; Git history and the pull request remain the historical record. Do not
preserve a document solely because another document links to it.

After moves or deletions, use `rg` to find the old path and, from the repository
root, run:

```sh
python3 .agents/skills/maintain-bitrouter-engineering-docs/scripts/check_local_links.py
```

Update skills that route to the changed document, but do not copy the document
into their `references/` directories.
