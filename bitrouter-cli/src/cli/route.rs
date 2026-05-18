//! CLI subcommand for managing runtime routes via the admin API.

use std::io::{self, Write};
use std::net::SocketAddr;

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::cli::admin_auth::{admin_get, parse_error_message, request_with_admin_auth};

/// Options for the `route add` subcommand.
pub struct RouteAddOpts {
    pub model: String,
    pub endpoints: Vec<String>,
    pub strategy: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteEndpoint {
    pub provider: String,
    pub service_id: String,
}

#[derive(Debug, Serialize)]
pub struct RouteEntry {
    pub name: String,
    pub source: String,
    pub endpoints: Vec<RouteEndpoint>,
    pub strategy: String,
}

#[derive(Debug, Serialize)]
pub struct RouteListData {
    pub routes: Vec<RouteEntry>,
}

pub fn query_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<RouteListData, Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/routes")?;
    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list routes: {msg}").into());
    }
    let body: serde_json::Value = resp.json()?;
    let raw_routes = body["routes"].as_array();
    let routes = match raw_routes {
        Some(raw) if !raw.is_empty() => raw
            .iter()
            .map(|route| {
                let name = route["name"].as_str().unwrap_or("?").to_owned();
                let source = route["source"].as_str().unwrap_or("?").to_owned();
                let strategy = route["strategy"].as_str().unwrap_or("").to_owned();
                let endpoints = route["endpoints"]
                    .as_array()
                    .map(|eps| {
                        eps.iter()
                            .map(|ep| RouteEndpoint {
                                provider: ep["provider"].as_str().unwrap_or("?").to_owned(),
                                service_id: ep["service_id"].as_str().unwrap_or("").to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                RouteEntry {
                    name,
                    source,
                    endpoints,
                    strategy,
                }
            })
            .collect(),
        _ => vec![],
    };
    Ok(RouteListData { routes })
}

pub fn render_list_text(data: &RouteListData, w: &mut impl Write) -> io::Result<()> {
    if data.routes.is_empty() {
        writeln!(w, "  (no routes configured)")?;
        return Ok(());
    }
    for route in &data.routes {
        let targets: Vec<String> = route
            .endpoints
            .iter()
            .map(|ep| {
                if ep.service_id.is_empty() {
                    ep.provider.clone()
                } else {
                    format!("{}:{}", ep.provider, ep.service_id)
                }
            })
            .collect();
        let strategy_suffix = if route.strategy.is_empty() {
            String::new()
        } else {
            format!(" ({})", route.strategy)
        };
        writeln!(
            w,
            "  {}  \u{2192}  {}{strategy_suffix}  [{}]",
            route.name,
            targets.join(", "),
            route.source
        )?;
    }
    Ok(())
}

/// Run the `route list` subcommand.
pub fn run_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = query_list(config, addr)?;
    match output {
        OutputFormat::Text => render_list_text(&data, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &data)?,
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

/// Run the `route delete` / `route rm` subcommand — removes a dynamic route.
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
