# bitrouter-api

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Reusable HTTP API surface for BitRouter.

This crate provides Warp filters and shared API helpers for serving
provider-compatible endpoints. It focuses on HTTP request handling and delegates
model resolution and execution to the routing contracts from `bitrouter-core`.

## Includes

- OpenAI-compatible routes
- Anthropic-compatible routes
- Google-compatible routes
- MCP-compatible routes
- A2A-compatible routes
- Shared API error and utility helpers

## Feature flags

- `openai`, `anthropic`, `google` enable provider-compatible HTTP surfaces.
- `mcp` enables the MCP routing surface and pulls in `bitrouter-mcp`.
- `a2a` enables the A2A routing surface and pulls in `bitrouter-a2a`.

Default features keep the current API surface enabled:
`openai`, `anthropic`, `google`, `mcp`, and `a2a`.
