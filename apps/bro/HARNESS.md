# BRO source guide

The executable in this package operates a local workspace and communicates
with a BitRouter gateway over the public SDK. Build it with
`cargo build --locked -p bitrouter-bro --bin bro`. For command examples, see
the [CLI reference](../../docs/CLI.md#local-bro-threads-and-agents).

| Follow a request | Source |
| --- | --- |
| Parse the CLI, select a workspace and load a local thread | [main.rs](src/main.rs) |
| Construct the model context and decode one streaming model step | [gateway.rs](src/gateway.rs) |
| Drive turns, record approvals, execute accepted tools and resume | [runner.rs](src/runner.rs) |
| Inspect or update checkpoints, locks and workspace admissions | [checkpoint.rs](src/checkpoint.rs) |
| Schedule local agents and reconcile parent/child state | [orchestration.rs](src/orchestration.rs) |
| Validate tool arguments, restrict filesystem access and supervise commands | [tools.rs](src/tools.rs) and [tool implementations](src/tools/) |
| Read and pin a selected skill | [skills.rs](src/skills.rs) |
| Start a gateway-bound adapter or supervise an explicit stdio adapter | [acp.rs](src/acp.rs) and [serve.rs](src/acp/serve.rs) |

Run `cargo test --locked -p bitrouter-bro` for the product tests. The
[cross-product process test](../bitrouter/tests/product_process.rs) also needs
a separately built BRO executable. The [distribution workflow](../../.github/workflows/distribution-acceptance.yml)
exercises that boundary with both products and a loopback provider.
