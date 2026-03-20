# bitrouter-mcp

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

MCP types, config, and traits for BitRouter — lightweight protocol library.

This crate provides the shared type definitions, configuration structures,
and traits for BitRouter's MCP (Model Context Protocol) gateway. It has no
dependency on `rmcp` or any async runtime and can be used independently for
configuration parsing or admin API integration. The runtime gateway
(upstream connections, tool aggregation, MCP server handler) lives in the
`bitrouter` binary crate behind the `mcp` feature gate.

## Includes

- `McpServerConfig` and `McpTransport` for upstream server configuration (stdio and HTTP) in `config`
- `ToolFilter` with deny-takes-precedence allowlist/denylist logic in `config`
- `ToolCostConfig` for per-server and per-tool cost tracking in `config`
- `ParamRestrictions` and `ParamRule` for parameter-level access control (strip or reject) in `param_filter`
- `McpAccessGroups` for named server groups with pattern expansion in `groups`
- `AdminToolRegistry` trait for runtime tool listing, filter mutation, and group introspection in `admin`
- `McpGatewayError` covering upstream, routing, config, param, and budget errors in `error`
