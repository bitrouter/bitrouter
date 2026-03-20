//! CLI subcommand for managing runtime MCP tools via the admin API.

use std::net::SocketAddr;
use std::path::Path;

use reqwest::blocking::{Client, RequestBuilder, Response};

use crate::cli::keygen::generate_local_admin_jwt;

/// Run the `tools list` subcommand — prints all tools from the running daemon.
pub fn run_list(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/tools");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

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
                let server = tool["server"].as_str().unwrap_or("?");
                let desc = tool["description"].as_str().unwrap_or("");
                let source = tool["source"].as_str().unwrap_or("?");

                if desc.is_empty() {
                    println!("  {name}  [{server}]  ({source})");
                } else {
                    println!("  {name}  [{server}]  ({source})  — {desc}");
                }
            }
        }
        _ => {
            println!("  (no tools available)");
        }
    }
    Ok(())
}

/// Run the `tools filter` subcommand — updates the tool filter for an upstream.
pub fn run_filter(
    keys_dir: &Path,
    addr: SocketAddr,
    server: &str,
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/tools/{server}/filter");
    let body = serde_json::json!({
        "allow": allow,
        "deny": deny,
    });
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.put(&url))?
        .json(&body)
        .send()?;

    if resp.status().is_success() {
        println!("filter updated for '{server}'");
    } else {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to update filter: {msg}").into());
    }
    Ok(())
}

/// Run the `tools upstreams` subcommand — lists upstream servers.
pub fn run_upstreams(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/tools/upstreams");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list upstreams: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let upstreams = body["upstreams"].as_array();
    match upstreams {
        Some(upstreams) if !upstreams.is_empty() => {
            for upstream in upstreams {
                let name = upstream["name"].as_str().unwrap_or("?");
                let tool_count = upstream["tool_count"].as_u64().unwrap_or(0);
                let has_filter = upstream["filter"].is_object();
                let filter_info = if has_filter { " (filtered)" } else { "" };
                println!("  {name}  {tool_count} tools{filter_info}");
            }
        }
        _ => {
            println!("  (no upstreams configured)");
        }
    }
    Ok(())
}

/// Run the `tools groups` subcommand — lists configured access groups.
pub fn run_groups(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/tools/groups");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list groups: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let groups = body["groups"].as_object();
    match groups {
        Some(groups) if !groups.is_empty() => {
            for (name, servers) in groups {
                let server_list = servers
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!("  {name}: [{server_list}]");
            }
        }
        _ => {
            println!("  (no groups configured)");
        }
    }
    Ok(())
}

fn request_with_admin_auth(
    keys_dir: &Path,
    request: RequestBuilder,
) -> Result<RequestBuilder, Box<dyn std::error::Error>> {
    let jwt = generate_local_admin_jwt(keys_dir)?;
    Ok(request.bearer_auth(jwt))
}

fn parse_error_message(response: Response) -> Result<String, Box<dyn std::error::Error>> {
    let status = response.status();
    let body = response.text()?;
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();

    if let Some(message) = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|value| value.get("message"))
        .and_then(serde_json::Value::as_str)
    {
        return Ok(message.to_owned());
    }

    if body.trim().is_empty() {
        Ok(format!("request failed with status {status}"))
    } else {
        Ok(format!("request failed with status {status}: {body}"))
    }
}
