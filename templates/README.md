# Routing templates

Ready-made **policy specs** for routing. Signed examples ship a frozen
`policy-lock.yaml`; initialization examples create one from models selected by
the operator.

Available templates:

- [`coding-router`](./coding-router/) — initialize the minimal
  `bitrouter/coding` router with models chosen by the operator.
- [`auto-router`](./auto-router/) — a legacy compatibility example for
  `bitrouter/auto` / `bitrouter/auto:cost`, with explicitly configured GPT-5.6
  and DeepSeek V4 Pro targets. It is not installed by default.

Want one for another workflow? Open an issue or email
[kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai).
