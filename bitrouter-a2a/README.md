# bitrouter-a2a

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

A2A (Agent-to-Agent) protocol types, traits, and client for BitRouter.

This crate implements the [A2A v0.3.0 specification](https://a2a-protocol.org/v0.3.0/specification/)
for agent identity, discovery, and communication. It has no dependency on
`bitrouter-core` and can be used as a standalone A2A library.

## Protocol Version

**A2A v0.3.0** — uses lowercase slash-separated method names (`message/send`, `tasks/get`),
lowercase `TaskState` values with hyphens (`input-required`), and `"kind"` discriminator tags.

## Supported Features

### JSON-RPC Methods (all via `POST /a2a`)

| Method | Status |
|--------|--------|
| `message/send` | Supported |
| `message/stream` | Supported (SSE) |
| `tasks/get` | Supported |
| `tasks/cancel` | Supported |
| `tasks/list` | Supported |
| `tasks/resubscribe` | Supported (SSE) |
| `agent/getAuthenticatedExtendedCard` | Supported |
| `tasks/pushNotificationConfig/set` | Supported |
| `tasks/pushNotificationConfig/get` | Supported |
| `tasks/pushNotificationConfig/list` | Supported |
| `tasks/pushNotificationConfig/delete` | Supported |

### REST Endpoints

| Endpoint | Status |
|----------|--------|
| `GET /.well-known/agent-card.json` | Supported |
| `GET /card` | Supported |
| `GET /extendedAgentCard` | Supported |
| `POST /message:send` | Supported |
| `POST /message:stream` | Supported (SSE) |
| `GET /tasks/{id}` | Supported |
| `GET /tasks` | Supported (with query params) |
| `POST /tasks/{id}:cancel` | Supported |
| `POST /tasks/{id}:subscribe` | Supported (SSE) |
| `POST /tasks/{taskId}/pushNotificationConfigs` | Supported |
| `GET /tasks/{taskId}/pushNotificationConfigs` | Supported |
| `GET /tasks/{taskId}/pushNotificationConfigs/{id}` | Supported |
| `DELETE /tasks/{taskId}/pushNotificationConfigs/{id}` | Supported |

### Discovery

- Agent card served at `/.well-known/agent-card.json` and `/card`
- Agent card change notifications via `subscribe_card_changes()`
- Authenticated extended card via JSON-RPC and REST

### Streaming

- SSE streaming for `message/stream` and `tasks/resubscribe`
- Both JSON-RPC and REST transports supported

### Transport

| Transport | Status |
|-----------|--------|
| HTTP (JSON-RPC + REST) | Supported |
| SSE streaming | Supported |
| gRPC | Not supported |

## Includes

- Agent Card types and security scheme definitions in `card` and `security`
- Task lifecycle types (Task, Message, Part, Artifact) in `task` and `message`
- JSON-RPC 2.0 wire format types in `jsonrpc`
- Server-side traits (`A2aDiscovery`, `A2aProxy`) in `server`
- Push notification config types and CRUD operations
- A2A protocol client for discovery and task dispatch in `client`
- Streaming response types in `stream`
