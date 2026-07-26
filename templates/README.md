# Workflow templates

Ready-made **policy specs** for common agentic workflows — a starting routing
configuration you can drop into a loop so it routes well before you tune it
yourself.

Each template targets a workflow (a harness plus a task type) and ships a
`bitrouter.yaml` plus its frozen policy lock. The pair routes calls, tools, and
agents across the cost / latency / accuracy objectives BitRouter optimizes for.

Available templates:

- [`superpowers-policy`](./superpowers-policy/) — contextual Codex +
  Superpowers routing with GPT-5.6 as the strong default and DeepSeek V4 Pro
  for reward-qualified mechanical agent contexts.

Want one for another workflow? Open an issue or email
[kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai).
