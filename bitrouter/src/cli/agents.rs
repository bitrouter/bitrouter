//! `bitrouter agents` subcommand — inspect upstream agents on a running daemon.

use std::net::SocketAddr;

use crate::cli::admin_auth::{admin_get, parse_error_message};

/// Run the `agents list` subcommand — prints all agents from the running daemon.
pub fn run_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/agents")?;

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
pub fn run_status(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/agents")?;

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
