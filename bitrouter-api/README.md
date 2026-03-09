# bitrouter-api

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Reusable HTTP API surface for BitRouter.

This crate provides Warp filters and shared API helpers for serving
provider-compatible endpoints. It focuses on HTTP request handling and delegates
model resolution and execution to the routing contracts from `bitrouter-core`.

## Includes

- OpenAI-compatible routes
- Anthropic-compatible routes
- Shared API error and utility helpers
