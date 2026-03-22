//! `bitrouter agents` subcommand — inspect upstream agents on a running daemon.

use std::net::SocketAddr;
use std::path::Path;

use reqwest::blocking::Client;

use crate::cli::tools::{parse_error_message, request_with_admin_auth};

/// Run the `agents list` subcommand — prints all agents from the running daemon.
pub fn run_list(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/agents");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list agents: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let agents = body["agents"].as_array();
    match agents {
        Some(agents) if !agents.is_empty() => {
            for agent in agents {
                let name = agent["name"].as_str().unwrap_or("?");
                let url = agent["url"].as_str().unwrap_or("?");
                println!("  {name}  {url}");
            }
        }
        _ => {
            println!("  (no agents configured)");
        }
    }
    Ok(())
}

/// Run the `agents status` subcommand — shows upstream agent connection health.
pub fn run_status(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/agents");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to get agent status: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let agents = body["agents"].as_array();
    match agents {
        Some(agents) if !agents.is_empty() => {
            for agent in agents {
                let name = agent["name"].as_str().unwrap_or("?");
                let url = agent["url"].as_str().unwrap_or("?");
                let connected = agent["connected"].as_bool().unwrap_or(false);
                let status = if connected {
                    "connected"
                } else {
                    "disconnected"
                };
                println!("  {name}    {status}    {url}");
            }
        }
        _ => {
            println!("  (no agents configured)");
        }
    }
    Ok(())
}
