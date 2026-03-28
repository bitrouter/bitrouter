# Unified Provider Config

Providers are capability-agnostic service identities. A single provider can serve both models and tools, speaking different protocols per endpoint.

References: [GitHub issue #205](https://github.com/bitrouter/bitrouter/issues/205)

## 1. API protocol

`ApiProtocol` is defined in `bitrouter-core` and covers both model and tool wire formats:

| Variant    | Service | Transport        |
|------------|---------|------------------|
| `openai`   | model   | HTTP             |
| `anthropic`| model   | HTTP             |
| `google`   | model   | HTTP             |
| `mcp`      | tool    | Streamable HTTP  |
| `a2a`      | tool    | HTTP             |
| `rest`     | tool    | HTTP             |
| `skill`    | tool    | context injection|

Protocol is **defaultable at the provider level, overridable per-endpoint**.

## 2. Provider config

All fields are `Option` to support partial overlays via `derives`:

```yaml
providers:
  anthropic:
    api_key: "${ANTHROPIC_API_KEY}"
    env_prefix: ANTHROPIC
    # No default api_protocol — speaks multiple protocols

  deepseek:
    api_protocol: openai
    api_base: "https://api.deepseek.com/v1"
    env_prefix: DEEPSEEK

  github-mcp:
    api_protocol: mcp
    api_base: "https://api.githubcopilot.com/mcp"
    api_key: "${GITHUB_TOKEN}"

  my-github:
    derives: github-mcp
    api_key: "${MY_GITHUB_TOKEN}"
```

Providers carry credentials (`api_key`, `api_base`, `env_prefix`, `default_headers`, `auth`), optional model metadata catalogs, and optional default `api_protocol`. The `derives` mechanism works identically for model and tool providers.

## 3. Model routing

Unchanged from prior design. Virtual model names map to provider endpoints:

```yaml
models:
  claude-sonnet-4:
    endpoints:
      - provider: anthropic
        model_id: claude-sonnet-4
        api_protocol: anthropic     # per-endpoint override
```

`ModelEndpoint` fields: `provider`, `model_id`, `api_protocol?`, `api_key?`, `api_base?`.

Protocol resolution: `endpoint.api_protocol > provider.api_protocol > error`.

## 4. Tool routing

New `tools:` section, structurally parallel to `models:`:

```yaml
tools:
  create_issue:
    strategy: priority
    endpoints:
      - provider: github-mcp
        tool_id: create_issue

  web_search:
    endpoints:
      - provider: anthropic
        tool_id: web_search
        api_protocol: mcp           # override: anthropic provider, mcp protocol
        api_base: "https://mcp.anthropic.com"
```

`ToolEndpoint` fields: `provider`, `tool_id`, `api_protocol?`, `api_key?`, `api_base?`.

`ToolConfig` fields: `strategy` (priority | load_balance), `endpoints`, `pricing?`.

### Resolution strategies

1. **Direct routing**: `"provider:tool_id"` routes to the named provider if it exists.
2. **Tool lookup**: Name is looked up in the `tools` map, with strategy-based endpoint selection.
3. **No default fallback**: Unlike models, tools must be explicitly configured or discovered. Bare names without a matching entry return an error.

### Pricing

Per-tool pricing is embedded in `ToolConfig.pricing` (optional `ToolPricing` with `default_cost_per_call` and per-tool overrides). This replaces the separate `mcp_server_pricing` and `a2a_agent_pricing` top-level sections.

## 5. Core traits

### `RoutingTable` (models, existing)

```rust
pub trait RoutingTable {
    fn route(&self, incoming_model_name: &str)
        -> impl Future<Output = Result<RoutingTarget>> + Send;
    fn list_routes(&self) -> Vec<RouteEntry>;
}
```

### `ToolRoutingTable` (tools, new)

```rust
pub trait ToolRoutingTable {
    fn route_tool(&self, tool_name: &str)
        -> impl Future<Output = Result<ToolRoutingTarget>> + Send;
    fn list_tool_routes(&self) -> Vec<ToolRouteEntry>;
}
```

`ToolRoutingTarget` carries `provider_name`, `tool_id`, and `api_protocol`.

## 6. Config implementations

- `ConfigRoutingTable` — implements `RoutingTable` + `ModelRegistry` for model routing from YAML config.
- `ConfigToolRoutingTable` — implements `ToolRoutingTable` for tool routing from YAML config.

Both share the same resolution logic (direct routing, map lookup, strategy-based endpoint selection) and the same protocol resolution (endpoint override > provider default).

## 7. What collapsed

The following top-level config sections are replaced by `providers` + `tools`:

| Old section          | New equivalent                                |
|----------------------|-----------------------------------------------|
| `mcp_servers`        | tool providers with `api_protocol: mcp`       |
| `a2a_agents`         | tool providers with `api_protocol: a2a`       |
| `skills`             | tool providers with `api_protocol: skill`     |
| `mcp_server_pricing` | `pricing` field on `ToolConfig`               |
| `a2a_agent_pricing`  | `pricing` field on `ToolConfig`               |
| `mcp_groups`         | TBD — access groups on tool routing           |

## 8. Implementation status

**Complete:**
- `ApiProtocol` enum in `bitrouter-core` with model + tool variants
- Per-endpoint `api_protocol` override on `ModelEndpoint`
- `ToolEndpoint`, `ToolConfig`, and `tools:` config section
- `ToolRoutingTable` trait in core
- `ConfigToolRoutingTable` implementation in config

**Pending:**
- Migrate binary crate (`server.rs`) to use `ConfigToolRoutingTable` instead of legacy fields
- Remove legacy `mcp_servers`, `a2a_agents`, `skills`, pricing fields from `BitrouterConfig`
- Remove old `ToolServerConfig`, `AgentConfig` upstream types from core
- MCP tool discovery at startup (call `tools/list`, auto-populate routing table)
