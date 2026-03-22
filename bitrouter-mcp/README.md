# bitrouter-mcp

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

MCP types and traits for BitRouter — lightweight protocol library.

This crate provides error definitions, server traits, and MCP-specific
types for BitRouter's MCP (Model Context Protocol) gateway. It uses pure
serde structs with no dependency on `rmcp`, allowing `bitrouter-api` to
serve the protocol independently.

## Protocol Version

**MCP 2025-11-25** — the latest stable specification.

## Supported Features

### Client-to-Server Methods

| Method | Status |
|--------|--------|
| `initialize` | Supported |
| `ping` | Supported |
| `tools/list` | Supported |
| `tools/call` | Supported |
| `resources/list` | Supported |
| `resources/read` | Supported |
| `resources/templates/list` | Supported |
| `resources/subscribe` | Supported |
| `resources/unsubscribe` | Supported |
| `prompts/list` | Supported |
| `prompts/get` | Supported |
| `logging/setLevel` | Supported |
| `completion/complete` | Supported |
| `tasks/get` | Not supported |
| `tasks/result` | Not supported |
| `tasks/cancel` | Not supported |
| `tasks/list` | Not supported |

### Notifications (Client-to-Server)

| Notification | Status |
|--------------|--------|
| `notifications/initialized` | Accepted |
| `notifications/cancelled` | Accepted |
| `notifications/progress` | Accepted |
| `notifications/roots/list_changed` | Accepted |

### Notifications (Server-to-Client)

| Notification | Status |
|--------------|--------|
| `notifications/tools/list_changed` | Supported (via SSE) |
| `notifications/resources/list_changed` | Supported (via SSE) |
| `notifications/prompts/list_changed` | Supported (via SSE) |
| `notifications/resources/updated` | Not supported |
| `notifications/tasks/status` | Not supported |
| `notifications/message` | Not supported |

### Server-to-Client Methods

| Method | Status |
|--------|--------|
| `sampling/createMessage` | Not supported |
| `roots/list` | Not supported |
| `elicitation/create` | Not supported |

### Capabilities Advertised

| Capability | Status |
|------------|--------|
| `tools` (with `listChanged`) | Supported |
| `resources` (with `listChanged`, `subscribe`) | Supported |
| `prompts` (with `listChanged`) | Supported |
| `logging` | Supported |
| `completions` | Supported |
| `tasks` | Not supported |

### Transport

| Transport | Status |
|-----------|--------|
| HTTP+SSE (`POST /mcp` + `GET /mcp/sse`) | Supported |
| Streamable HTTP (`POST/GET/DELETE /mcp` with sessions) | Not supported |
| stdio | Not applicable (proxy architecture) |

## Roadmap

### Tasks Support
MCP tasks (`tasks/get`, `tasks/result`, `tasks/cancel`, `tasks/list`,
`notifications/tasks/status`) require stateful session management. This
will be implemented alongside the Streamable HTTP transport.

### Streamable HTTP Transport
Full Streamable HTTP with `Mcp-Session-Id` session management, `DELETE /mcp`
for session termination, `Accept: text/event-stream` content negotiation on
`POST /mcp`, and `Last-Event-ID` reconnection support. Required for
server-to-client method forwarding.

### Server-to-Client Method Forwarding
Forwarding `sampling/createMessage`, `roots/list`, and `elicitation/create`
from upstream MCP servers to downstream clients. Requires Streamable HTTP
sessions for bidirectional request/response correlation.

### Per-Resource Update Notifications
`notifications/resources/updated` for subscribed resources. Currently,
subscriptions are accepted but only list-level change notifications are emitted.

## Includes

- `McpGatewayError` covering upstream, routing, config, param, subscription, and completion errors in `error`
- Server traits (`McpToolServer`, `McpResourceServer`, `McpPromptServer`, `McpSubscriptionServer`, `McpLoggingServer`, `McpCompletionServer`) in `server`
- MCP-specific types (`McpTool`, `McpResource`, `McpPrompt`, logging, completion, notification params, etc.) in `types`
- Client-side upstream connection registry in `client`
