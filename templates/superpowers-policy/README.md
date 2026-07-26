# Superpowers contextual policy

This template keeps GPT-5.6 as the strong default and routes only
reward-qualified mechanical Superpowers contexts to DeepSeek V4 Pro.

Start BitRouter from this directory:

```bash
bitrouter serve --config bitrouter.yaml
```

Point Codex Responses traffic at the `superpowers` preset and propagate the
following allowlisted context headers:

```text
x-bitrouter-agent-role
x-bitrouter-task-id
x-bitrouter-review-kind
x-bitrouter-task-complexity
```

The frozen policy lock is intentionally conservative: unknown contexts,
controllers, standard/complex work, and tool use fall back to GPT-5.6.

The static lock cannot enforce trajectory rules by itself. The calling agent
integration should maintain per-role/task turn counts, escalate beyond the
budgets in `policy-metadata.json`, and classify final, final-fix, and re-review
contexts as `standard` from their first request. Those safeguards were part of
the evaluated policy bundle.

The recorded three-run result proves stability within one audited scenario,
not across scenarios. Treat this as a strong starting policy and re-evaluate it
against your own task distribution before expanding the economy routes.
