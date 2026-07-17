//! The gateway MCP servers injected into TUI-launched harnesses.
//!
//! Two of BitRouter's four gateways reach a launched harness as injected MCP
//! servers (the other two — models and ACP — ride the routing overlay and
//! the fleet bridge):
//!
//! - **`bitrouter_tools`** — the MCP gateway: the daemon's aggregate endpoint
//!   (`mcp.aggregate.route`, default `POST /mcp`), which fans out to every
//!   configured `mcp_servers` upstream with `{server}__` tool prefixes.
//!   Injected as a streamable-HTTP server so the harness's own MCP client
//!   dials the daemon directly.
//! - **`bitrouter_skills`** — the AgentSkills gateway: this binary running
//!   `mcp serve --backend skills` (stdio), serving `skills_search` /
//!   `skills_get` over the installed-skills root.
//!
//! Both harness roles get the same pair — the interactive orchestrator via
//! config synthesis ([`crate::harness::Harness::orchestrator_overlay`]) and
//! ACP subagents via `session/new` `mcpServers` descriptors ([`to_acp`]).
//! One spec, two renderers, so the roles can't drift.

use agent_client_protocol::schema::v1 as acp;

use crate::harness::{McpServer, McpTransport};

/// The gateway servers for a daemon at `base_url`, authenticating with
/// `auth`. `aggregate_route` is the daemon's aggregate MCP path
/// (`mcp.aggregate.route`); `None` (aggregate disabled) omits the
/// `bitrouter_tools` server.
pub fn gateway_servers(
    base_url: &str,
    auth: &str,
    aggregate_route: Option<&str>,
) -> Vec<McpServer> {
    let mut servers = Vec::new();
    if let Some(route) = aggregate_route {
        servers.push(McpServer {
            name: "bitrouter_tools".to_string(),
            transport: McpTransport::Http {
                url: join_route(base_url, route),
                // Same convention as the models routing overlay: always send
                // the credential — ignored by the daemon under `skip_auth:
                // true` (the local default), validated when auth is on.
                headers: vec![("Authorization".to_string(), format!("Bearer {auth}"))],
            },
        });
    }
    servers.push(McpServer {
        name: "bitrouter_skills".to_string(),
        transport: McpTransport::Stdio {
            command: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "bitrouter".to_string()),
            args: ["mcp", "serve", "--backend", "skills"]
                .map(str::to_string)
                .to_vec(),
        },
    });
    servers
}

/// Render a harness-facing server spec as the ACP `session/new` `mcpServers`
/// descriptor for a spawned subagent. Descriptor headers are the ACP wire
/// shape — an array of `{name, value}` — where harness config files carry an
/// object; both render from the same [`McpTransport`].
pub fn to_acp(server: &McpServer) -> acp::McpServer {
    match &server.transport {
        McpTransport::Stdio { command, args } => acp::McpServer::Stdio(
            acp::McpServerStdio::new(server.name.clone(), command.clone()).args(args.clone()),
        ),
        McpTransport::Http { url, headers } => acp::McpServer::Http(
            acp::McpServerHttp::new(server.name.clone(), url.clone()).headers(
                headers
                    .iter()
                    .map(|(name, value)| acp::HttpHeader::new(name.clone(), value.clone()))
                    .collect(),
            ),
        ),
    }
}

/// Join the daemon base URL and the aggregate route without doubling or
/// dropping the separating slash.
fn join_route(base_url: &str, route: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if route.starts_with('/') {
        format!("{base}{route}")
    } else {
        format!("{base}/{route}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_rides_the_aggregate_route_with_bearer_auth() {
        let servers = gateway_servers("http://127.0.0.1:4356/", "tok", Some("/mcp"));
        assert_eq!(servers.len(), 2);
        // Serialize the ACP rendering to lock the wire shape: tagged http
        // variant, headers as a {name, value} array.
        let wire = serde_json::to_value(to_acp(&servers[0])).expect("serialize");
        assert_eq!(wire["type"], "http");
        assert_eq!(wire["name"], "bitrouter_tools");
        assert_eq!(wire["url"], "http://127.0.0.1:4356/mcp");
        assert_eq!(wire["headers"][0]["name"], "Authorization");
        assert_eq!(wire["headers"][0]["value"], "Bearer tok");
    }

    #[test]
    fn skills_is_stdio_and_survives_a_disabled_aggregate() {
        let servers = gateway_servers("http://127.0.0.1:4356", "tok", None);
        assert_eq!(servers.len(), 1, "aggregate off drops bitrouter_tools");
        let wire = serde_json::to_value(to_acp(&servers[0])).expect("serialize");
        assert_eq!(wire["name"], "bitrouter_skills");
        assert!(wire.get("type").is_none(), "stdio is the untagged variant");
        assert_eq!(
            wire["args"],
            serde_json::json!(["mcp", "serve", "--backend", "skills"])
        );
    }

    #[test]
    fn join_route_normalizes_slashes() {
        assert_eq!(join_route("http://x:1/", "/mcp"), "http://x:1/mcp");
        assert_eq!(join_route("http://x:1", "mcp"), "http://x:1/mcp");
    }
}
