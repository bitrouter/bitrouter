//! CLI subcommand for managing runtime routes via the admin API.

use std::net::SocketAddr;

use crate::cli::admin_auth::{admin_get, parse_error_message, request_with_admin_auth};

/// Options for the `route add` subcommand.
pub struct RouteAddOpts {
    pub model: String,
    pub endpoints: Vec<String>,
    pub strategy: Option<String>,
}

/// Run the `route list` subcommand — prints all routes from the running daemon.
pub fn run_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/routes")?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list routes: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let routes = body["routes"].as_array();
    match routes {
        Some(routes) if !routes.is_empty() => {
            for route in routes {
                let model = route["model"].as_str().unwrap_or("?");
                let source = route["source"].as_str().unwrap_or("?");
                let endpoints = route["endpoints"].as_array();

                let targets: Vec<String> = endpoints
                    .map(|eps| {
                        eps.iter()
                            .map(|ep| {
                                let provider = ep["provider"].as_str().unwrap_or("?");
                                let model_id = ep["model_id"].as_str().unwrap_or("?");
                                if model_id.is_empty() {
                                    provider.to_owned()
                                } else {
                                    format!("{provider}:{model_id}")
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let strategy = route["strategy"].as_str().unwrap_or("");
                let strategy_suffix = if strategy.is_empty() {
                    String::new()
                } else {
                    format!(" ({strategy})")
                };

                println!(
                    "  {model}  →  {}{strategy_suffix}  [{source}]",
                    targets.join(", ")
                );
            }
        }
        _ => {
            println!("  (no routes configured)");
        }
    }
    Ok(())
}

/// Run the `route add` subcommand — creates or updates a dynamic route.
pub fn run_add(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
    opts: RouteAddOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    let endpoints: Vec<serde_json::Value> = opts
        .endpoints
        .iter()
        .map(|ep| {
            let (provider, model_id) = ep.split_once(':').unwrap_or((ep, ""));
            serde_json::json!({
                "provider": provider,
                "model_id": model_id,
            })
        })
        .collect();

    let body = serde_json::json!({
        "model": opts.model,
        "strategy": opts.strategy.unwrap_or_else(|| "priority".to_owned()),
        "endpoints": endpoints,
    });

    let url = format!("http://{addr}/admin/routes");
    let client = reqwest::blocking::Client::new();
    let resp = request_with_admin_auth(config, client.post(&url))?
        .json(&body)
        .send()?;

    if resp.status().is_success() {
        println!("route '{}' added", opts.model);
    } else {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to add route: {msg}").into());
    }
    Ok(())
}

/// Run the `route rm` subcommand — removes a dynamic route.
pub fn run_remove(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/routes/{name}");
    let client = reqwest::blocking::Client::new();
    let resp = request_with_admin_auth(config, client.delete(&url))?.send()?;

    if resp.status().is_success() {
        println!("route '{name}' removed");
    } else {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to remove route: {msg}").into());
    }
    Ok(())
}
