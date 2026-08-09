# Routing templates

Ready-made **policy specs** for adaptive routing. Each template ships a
`bitrouter.yaml` and frozen `policy-lock.yaml` that you can evaluate against
your own traffic before tuning it further.

Available templates:

- [`auto-router`](./auto-router/) — generic `bitrouter/auto` / `bitrouter/auto:cost` routing
  with GPT-5.6 as the strong default and DeepSeek V4 Pro for normal mechanical
  trace projections.

Want one for another workflow? Open an issue or email
[kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai).
