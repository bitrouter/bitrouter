//! CLI subcommand for managing runtime routes via the admin API.

use std::net::SocketAddr;
use std::path::Path;

use reqwest::blocking::{Client, RequestBuilder, Response};

use crate::cli::keygen::generate_local_admin_jwt;

/// Options for the `route add` subcommand.
pub struct RouteAddOpts {
    pub model: String,
    pub endpoints: Vec<String>,
    pub strategy: Option<String>,
}

/// Run the `route list` subcommand — prints all routes from the running daemon.
pub fn run_list(keys_dir: &Path, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/routes");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.get(&url))?.send()?;

    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list routes: {msg}").into());
    }

    let body: serde_json::Value = resp.json()?;
    let routes = body["routes"].as_array();
    match routes {
        Some(routes) if !routes.is_empty() => {
            for route in routes {
                let model = route["name"].as_str().unwrap_or("?");
                let source = route["source"].as_str().unwrap_or("?");
                let endpoints = route["endpoints"].as_array();

                let targets: Vec<String> = endpoints
                    .map(|eps| {
                        eps.iter()
                            .map(|ep| {
                                let provider = ep["provider"].as_str().unwrap_or("?");
                                let service_id = ep["service_id"].as_str().unwrap_or("?");
                                if service_id.is_empty() {
                                    provider.to_owned()
                                } else {
                                    format!("{provider}:{service_id}")
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
    keys_dir: &Path,
    addr: SocketAddr,
    opts: RouteAddOpts,
) -> Result<(), Box<dyn std::error::Error>> {
    let endpoints: Vec<serde_json::Value> = opts
        .endpoints
        .iter()
        .map(|ep| {
            let (provider, service_id) = ep.split_once(':').unwrap_or((ep, ""));
            serde_json::json!({
                "provider": provider,
                "service_id": service_id,
            })
        })
        .collect();

    let body = serde_json::json!({
        "name": opts.model,
        "strategy": opts.strategy.unwrap_or_else(|| "priority".to_owned()),
        "endpoints": endpoints,
    });

    let url = format!("http://{addr}/admin/routes");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.post(&url))?
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
    keys_dir: &Path,
    addr: SocketAddr,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{addr}/admin/routes/{name}");
    let client = Client::new();
    let resp = request_with_admin_auth(keys_dir, client.delete(&url))?.send()?;

    if resp.status().is_success() {
        println!("route '{name}' removed");
    } else {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to remove route: {msg}").into());
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
