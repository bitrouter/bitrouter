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
- Shared API error and utility helpers

## Feature flags

- `openai`, `anthropic`, `google` enable provider-compatible HTTP surfaces.
- `mcp` enables the MCP routing surface.

Default features keep the current API surface enabled:
`openai`, `anthropic`, `google`, and `mcp`.
