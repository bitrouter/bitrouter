# bitrouter-acp

Agent Client Protocol (ACP) integration for [BitRouter](https://github.com/bitrouter/bitrouter).

This crate hosts the ACP runtime (subprocess transport, session pool, permission bridge), the agent registry/install/state machinery, and the stdio proxy used by editor integrations. It implements the `AgentProvider` trait from `bitrouter-core::agents`.

It is consumed by the `bitrouter` runtime crate (under the `acp` feature) and by `bitrouter-cli` (for the `agents`, `agent-proxy`, and `agent` subcommands).
