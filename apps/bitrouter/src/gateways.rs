//! The gateway MCP servers injected into launched harnesses.
//!
//! BitRouter's upstream-tool gateway reaches a launched harness as an injected
//! MCP server (the models gateway instead rides the routing overlay):
//!
//! - **`bitrouter_tools`** — the MCP gateway: the daemon's aggregate endpoint
//!   (`mcp.aggregate.route`, default `POST /mcp`), which fans out to every
//!   configured `mcp_servers` upstream with `{server}__` tool prefixes.
//!   Injected as a streamable-HTTP server so the harness's own MCP client
//!   dials the daemon directly.
//!
//! [`gateway_servers`] is the one spec; [`to_acp`] renders it as the ACP
//! `session/new` `mcpServers` descriptor for a headless sub-agent, while
//! [`crate::harness::Harness::launch_overlay`] renders it into whatever config
//! surface an interactive harness offers. Two renderers, one source, so the
//! two paths can't drift.

use agent_client_protocol::schema::v1 as acp;

use crate::harness::{McpServer, McpTransport};

/// Name of the aggregate tool-gateway MCP server injected into a harness.
pub const TOOLS_SERVER: &str = "bitrouter_tools";
/// The gateway servers for a daemon at `base_url`, authenticating with
/// `auth`. `aggregate_route` is the daemon's aggregate MCP path
/// (`mcp.aggregate.route`); `None` (aggregate disabled) omits the
/// [`TOOLS_SERVER`].
pub fn gateway_servers(
    base_url: &str,
    auth: &str,
    aggregate_route: Option<&str>,
) -> Vec<McpServer> {
    aggregate_route
        .map(|route| McpServer {
            name: TOOLS_SERVER.to_string(),
            transport: McpTransport::Http {
                url: join_route(base_url, route),
                // Same convention as the models routing overlay: always send
                // the credential — ignored by the daemon under `skip_auth:
                // true` (the local default), validated when auth is on.
                headers: vec![("Authorization".to_string(), format!("Bearer {auth}"))],
            },
        })
        .into_iter()
        .collect()
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
        assert_eq!(servers.len(), 1);
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
    fn disabled_aggregate_injects_no_gateway() {
        let servers = gateway_servers("http://127.0.0.1:4356", "tok", None);
        assert!(servers.is_empty());
    }

    #[test]
    fn join_route_normalizes_slashes() {
        assert_eq!(join_route("http://x:1/", "/mcp"), "http://x:1/mcp");
        assert_eq!(join_route("http://x:1", "mcp"), "http://x:1/mcp");
    }
}
