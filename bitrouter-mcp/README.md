# bitrouter-mcp

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

MCP types and traits for BitRouter — lightweight protocol library.

This crate provides error definitions, server traits, and MCP-specific
types for BitRouter's MCP (Model Context Protocol) gateway. Configuration
types (`ToolServerConfig`, `ToolServerTransport`, etc.) live in
`bitrouter_core::routers::upstream`. The runtime gateway (upstream connections, tool
aggregation, MCP server handler) lives in the `bitrouter` binary crate
behind the `mcp` feature gate.

## Includes

- `McpGatewayError` covering upstream, routing, config, param, and budget errors in `error`
- Server traits (`McpToolServer`, `McpResourceServer`, `McpPromptServer`) in `server`
- MCP-specific types (`McpTool`, `McpResource`, `McpPrompt`, etc.) in `types`
