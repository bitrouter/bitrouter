//! `bitrouter tools` — introspection over the configured MCP servers.
//!
//! Three verbs:
//! - `list` — aggregate `tools/list` across every server in `mcp_servers`.
//! - `status` — health-check each server (dial + initialise).
//! - `discover <server>` — connect to one server and emit a config stub a
//!   user can paste into `bitrouter.yaml`.
//!
//! These are one-shot CLI invocations: each spins up a fresh
//! [`RmcpExecutor`], dials each server once, and exits — no daemon required.
//! See the spec for the dispatched methods:
//! <https://modelcontextprotocol.io/specification/2025-06-18>.

use std::time::{Duration, Instant};

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::Config;
use bitrouter_sdk::mcp::rmcp_executor::RmcpExecutor;
use bitrouter_sdk::mcp::transport::McpTransport;
use bitrouter_sdk::mcp::{Executor, McpRequest, McpTarget};

/// One row in `bitrouter tools list`.
#[derive(Debug, Clone)]
pub struct ServerTools {
    /// The server id (matches the `mcp_servers` key in `bitrouter.yaml`).
    pub server: String,
    /// Result of the `tools/list` call against that server.
    pub outcome: Result<Vec<ToolSummary>, String>,
}

/// One tool entry under a server.
#[derive(Debug, Clone)]
pub struct ToolSummary {
    /// The tool name as returned by the upstream.
    pub name: String,
    /// Optional human-readable description (empty if the server omits it).
    pub description: String,
}

/// One row in `bitrouter tools status`.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    /// The server id.
    pub server: String,
    /// Transport shape (`http <url>` or `stdio <command>`), surfaced so the
    /// operator can correlate failures with the entry's config.
    pub transport: String,
    /// `Ok(handshake_duration)` on a successful dial + `tools/list`; the
    /// duration is wall-clock from `execute()` call to first byte of result.
    /// `Err(message)` on any failure (spawn, network, handshake, dispatch).
    pub outcome: Result<Duration, String>,
}

/// `bitrouter tools list` — aggregate `tools/list` across every configured
/// MCP server. Errors from one server don't stop the rest.
pub async fn list(config: &Config) -> Vec<ServerTools> {
    let executor = RmcpExecutor::new();
    let mut out = Vec::with_capacity(config.mcp_servers.len());
    // Iterate in sorted order so the CLI output is stable across runs.
    let mut servers: Vec<_> = config.mcp_servers.iter().collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));
    for (name, server_cfg) in servers {
        let target = McpTarget::Direct {
            server_name: name.clone(),
            transport: server_cfg.transport.clone(),
        };
        let req = McpRequest::direct(
            name,
            "tools/list",
            serde_json::json!({}),
            CallerContext::local(),
        );
        let outcome = match executor.execute(&target, &req).await {
            Ok(resp) => parse_tools(&resp.result).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        out.push(ServerTools {
            server: name.clone(),
            outcome,
        });
    }
    out
}

/// `bitrouter tools status` — health-check each configured server with a
/// `tools/list` round-trip. Wall-clock latency surfaced for operators
/// triaging slow upstreams.
pub async fn status(config: &Config) -> Vec<ServerStatus> {
    let executor = RmcpExecutor::new();
    let mut out = Vec::with_capacity(config.mcp_servers.len());
    let mut servers: Vec<_> = config.mcp_servers.iter().collect();
    servers.sort_by(|a, b| a.0.cmp(b.0));
    for (name, server_cfg) in servers {
        let target = McpTarget::Direct {
            server_name: name.clone(),
            transport: server_cfg.transport.clone(),
        };
        let req = McpRequest::direct(
            name,
            "tools/list",
            serde_json::json!({}),
            CallerContext::local(),
        );
        let started = Instant::now();
        let outcome = match executor.execute(&target, &req).await {
            Ok(_) => Ok(started.elapsed()),
            Err(e) => Err(e.to_string()),
        };
        out.push(ServerStatus {
            server: name.clone(),
            transport: describe_transport(&server_cfg.transport),
            outcome,
        });
    }
    out
}

/// `bitrouter tools discover <server>` — connect to one server and emit a
/// commented YAML stub that previews its tool surface. Output is meant to
/// be piped into / pasted under a `mcp_servers:` entry.
pub async fn discover(config: &Config, server: &str) -> Result<String, String> {
    let server_cfg = config
        .mcp_servers
        .get(server)
        .ok_or_else(|| format!("no mcp server configured for '{server}'"))?;
    let executor = RmcpExecutor::new();
    let target = McpTarget::Direct {
        server_name: server.to_string(),
        transport: server_cfg.transport.clone(),
    };
    let req = McpRequest::direct(
        server,
        "tools/list",
        serde_json::json!({}),
        CallerContext::local(),
    );
    let resp = executor
        .execute(&target, &req)
        .await
        .map_err(|e| e.to_string())?;
    let tools = parse_tools(&resp.result).map_err(|e| e.to_string())?;
    Ok(render_discovery(server, &server_cfg.transport, &tools))
}

fn parse_tools(result: &serde_json::Value) -> Result<Vec<ToolSummary>, String> {
    let arr = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "upstream response missing `tools` array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "tool entry missing `name`".to_string())?
            .to_string();
        let description = entry
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(ToolSummary { name, description });
    }
    Ok(out)
}

fn describe_transport(t: &McpTransport) -> String {
    match t {
        McpTransport::Http { url, .. } => format!("http {url}"),
        McpTransport::Stdio { command, args, .. } => {
            if args.is_empty() {
                format!("stdio {command}")
            } else {
                format!("stdio {command} {}", args.join(" "))
            }
        }
    }
}

fn render_discovery(server: &str, transport: &McpTransport, tools: &[ToolSummary]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Discovered {} tool(s) from '{}' via {}.\n",
        tools.len(),
        server,
        describe_transport(transport)
    ));
    out.push_str("# Paste under `mcp_servers:` in bitrouter.yaml.\n");
    out.push_str(&format!("# {}\n", "-".repeat(60)));
    out.push_str(&format!("{server}:\n"));
    out.push_str(&format!("  name: {server}\n"));
    match transport {
        McpTransport::Http { url, .. } => {
            out.push_str("  transport:\n");
            out.push_str("    type: http\n");
            out.push_str(&format!("    url: {url}\n"));
        }
        McpTransport::Stdio { command, args, .. } => {
            out.push_str("  transport:\n");
            out.push_str("    type: stdio\n");
            out.push_str(&format!("    command: {command}\n"));
            if !args.is_empty() {
                out.push_str("    args:\n");
                for a in args {
                    out.push_str(&format!("      - {a}\n"));
                }
            }
        }
    }
    if tools.is_empty() {
        out.push_str("# (no tools advertised)\n");
        return out;
    }
    out.push_str("# tools (informational — not part of the config schema):\n");
    for t in tools {
        if t.description.is_empty() {
            out.push_str(&format!("#   - {}\n", t.name));
        } else {
            let one_line = t.description.replace('\n', " ");
            out.push_str(&format!("#   - {} — {}\n", t.name, one_line));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::mcp::transport::McpServerConfig;
    use std::collections::HashMap;

    fn cfg_with(server_id: &str, server_cfg: McpServerConfig) -> Config {
        let mut c = Config::default();
        c.mcp_servers.insert(server_id.to_string(), server_cfg);
        c
    }

    fn stdio_target(server: &str, cmd: &str) -> McpServerConfig {
        McpServerConfig::with_defaults(
            server,
            McpTransport::Stdio {
                command: cmd.into(),
                args: vec![],
                env: HashMap::new(),
            },
        )
    }

    #[tokio::test]
    async fn list_reports_per_server_errors_independently() {
        let mut config = Config::default();
        config
            .mcp_servers
            .insert("a".into(), stdio_target("a", "/bin/false"));
        config
            .mcp_servers
            .insert("b".into(), stdio_target("b", "/bin/false"));
        let rows = list(&config).await;
        assert_eq!(rows.len(), 2);
        // Both fail (false exits with no MCP handshake), but each row is
        // independently populated and ordered alphabetically.
        assert_eq!(rows[0].server, "a");
        assert_eq!(rows[1].server, "b");
        assert!(rows[0].outcome.is_err());
        assert!(rows[1].outcome.is_err());
    }

    #[tokio::test]
    async fn status_includes_transport_description_and_error() {
        let config = cfg_with("a", stdio_target("a", "/bin/false"));
        let rows = status(&config).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server, "a");
        assert_eq!(rows[0].transport, "stdio /bin/false");
        assert!(rows[0].outcome.is_err());
    }

    #[tokio::test]
    async fn discover_unknown_server_is_user_facing_error() {
        let config = Config::default();
        let err = discover(&config, "missing").await.unwrap_err();
        assert!(err.contains("no mcp server configured for 'missing'"));
    }

    #[tokio::test]
    async fn discover_propagates_transport_failure() {
        let config = cfg_with("a", stdio_target("a", "/bin/false"));
        let err = discover(&config, "a").await.unwrap_err();
        assert!(err.contains("mcp 'a'"), "unexpected: {err}");
    }

    #[test]
    fn render_discovery_emits_paste_ready_yaml_stub() {
        let transport = McpTransport::Http {
            url: "https://x".into(),
            headers: Default::default(),
        };
        let tools = vec![
            ToolSummary {
                name: "search".into(),
                description: "Search the docs".into(),
            },
            ToolSummary {
                name: "fetch".into(),
                description: String::new(),
            },
        ];
        let out = render_discovery("ctx7", &transport, &tools);
        assert!(out.contains("ctx7:"));
        assert!(out.contains("type: http"));
        assert!(out.contains("url: https://x"));
        assert!(out.contains("- search — Search the docs"));
        assert!(out.contains("- fetch\n"));
    }

    #[test]
    fn render_discovery_handles_empty_tools_list() {
        let transport = McpTransport::Stdio {
            command: "uvx".into(),
            args: vec!["mcp-server-git".into()],
            env: Default::default(),
        };
        let out = render_discovery("git", &transport, &[]);
        assert!(out.contains("git:"));
        assert!(out.contains("command: uvx"));
        assert!(out.contains("- mcp-server-git"));
        assert!(out.contains("(no tools advertised)"));
    }

    #[test]
    fn parse_tools_rejects_missing_array() {
        let bad = serde_json::json!({"not_tools": []});
        assert!(parse_tools(&bad).is_err());
    }

    #[test]
    fn parse_tools_rejects_entry_missing_name() {
        let bad = serde_json::json!({"tools": [{"description": "x"}]});
        assert!(parse_tools(&bad).is_err());
    }

    #[test]
    fn parse_tools_accepts_missing_description() {
        let ok = serde_json::json!({"tools": [{"name": "search"}]});
        let parsed = parse_tools(&ok).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "search");
        assert_eq!(parsed[0].description, "");
    }
}
