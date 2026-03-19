# bitrouter-a2a

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

A2A (Agent-to-Agent) protocol types, traits, and client for BitRouter.

This crate implements the [A2A v1.0 specification](https://a2a-protocol.org/latest/)
for agent identity, discovery, and communication. It has no dependency on
`bitrouter-core` and can be used as a standalone A2A library.

## Includes

- Agent Card types and security scheme definitions in `card` and `security`
- Task lifecycle types (Task, Message, Part, Artifact) in `task` and `message`
- JSON-RPC 2.0 wire format types in `jsonrpc`
- Server-side traits (`AgentExecutor`, `TaskStore`, `PushNotificationStore`) in `server`
- Agent card registry trait in `registry`
- A2A protocol client for discovery and task dispatch in `client`
- Streaming response types in `stream`
