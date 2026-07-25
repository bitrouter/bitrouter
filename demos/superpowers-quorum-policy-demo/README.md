# Superpowers + Quorum policy demo

This is a small, local demo of the `sdd-quality-reviewer-catches-planted-defect` scenario. It trains a contextual safe-key policy, freezes and hashes it, then runs isolated Codex-control, Superpowers-adaptive, and Quorum-reviewer arms behind a shared barrier. The verifier reads raw traces and independently recomputes joins, rewards, prices, overlap, and policy provenance.

The one documented setup/run command is:

```bash
bun install --cwd demos/superpowers-quorum-policy-demo --frozen-lockfile && bun demos/superpowers-quorum-policy-demo/src/cli.ts fixture && bun demos/superpowers-quorum-policy-demo/src/cli.ts verify artifacts/superpowers-policy-demo/latest
```

`fixture` is deterministic and explicitly simulated; its bundle can never set `realExecution`. `smoke` and `full` use the local ingress/arm runner and set `realExecution` only after that runner completes:

```bash
bun demos/superpowers-quorum-policy-demo/src/cli.ts smoke
bun demos/superpowers-quorum-policy-demo/src/cli.ts full
```

The control arm is constrained to `openai-codex:gpt-5.6-sol`; adaptive routing may choose only that model or `bitrouter:moonshotai/kimi-k2.7-code`, with unsafe phases falling back to the strong model. The verifier requires exact request-ID joins across ingress, decisions, usage, contexts, trajectories, and outcomes, then independently recomputes rewards and cache-aware costs from raw tables and the frozen price snapshot. Subscription marginal spend remains unobservable and separate.

Run tests with `bun test test`. The static `dashboard.html` is artifact-only and has no network dependency.
