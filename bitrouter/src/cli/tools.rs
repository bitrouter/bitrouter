//! `bitrouter tools` subcommand — inspect MCP tools on a running daemon.

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
pub fn run_status(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/tools/upstreams");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to get tool status: {msg}").into());
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
                println!("  {name}    {tool_count} tools{filter_info}");
            }
        }
        _ => {
            println!("  (no upstreams configured)");
        }
    }
    Ok(())
}

pub(crate) fn request_with_admin_auth(
    keys_dir: &Path,
    request: RequestBuilder,
) -> Result<RequestBuilder, Box<dyn std::error::Error>> {
    let jwt = generate_local_admin_jwt(keys_dir)?;
    Ok(request.bearer_auth(jwt))
}

pub(crate) fn parse_error_message(
    response: Response,
) -> Result<String, Box<dyn std::error::Error>> {
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
