//! `bitrouter tools` subcommand — inspect MCP tools and servers on a running daemon.

use std::net::SocketAddr;

use crate::cli::admin_auth::{admin_get, parse_error_message};

/// Run the `tools list` subcommand — prints all tools from the running daemon.
pub fn run_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/tools")?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list tools: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let tools = body["tools"].as_array();
    match tools {
        Some(tools) if !tools.is_empty() => {
            for tool in tools {
                let name = tool["name"].as_str().unwrap_or("?");
                let server = tool["provider"].as_str().unwrap_or("?");
                let desc = tool["description"].as_str().unwrap_or("");

                if desc.is_empty() {
                    println!("  {name}  [{server}]");
                } else {
                    println!("  {name}  [{server}]  — {desc}");
                }
            }
        }
        _ => {
            println!("  (no tools available)");
        }
    }
    Ok(())
}

/// Run the `tools status` subcommand — shows upstream server health.
pub fn run_status(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/tools/upstreams")?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to get tool status: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let servers = body["servers"].as_array();
    match servers {
        Some(servers) if !servers.is_empty() => {
            for server in servers {
                let name = server["name"].as_str().unwrap_or("?");
                let tool_count = server["tool_count"].as_u64().unwrap_or(0);
                let has_filter = server["filter"].is_object();
                let filter_info = if has_filter { " (filtered)" } else { "" };
                println!("  {name}    {tool_count} tools{filter_info}");
            }
        }
        _ => {
            println!("  (no MCP servers configured)");
        }
    }
    Ok(())
}
