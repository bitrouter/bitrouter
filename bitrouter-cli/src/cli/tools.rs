//! `bitrouter tools` subcommand — inspect MCP tools and servers on a running daemon,
//! and discover tools from MCP upstreams for config authoring.

use std::io::{self, Write};
use std::net::SocketAddr;

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::cli::admin_auth::{admin_get, parse_error_message};

#[derive(Debug, Serialize)]
pub struct ToolEntry {
    pub name: String,
    pub provider: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ToolListData {
    pub tools: Vec<ToolEntry>,
}

#[derive(Debug, Serialize)]
pub struct ServerEntry {
    pub name: String,
    pub tool_count: u64,
    pub filtered: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolStatusData {
    pub servers: Vec<ServerEntry>,
}

pub fn query_list(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<ToolListData, Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/tools")?;
    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to list tools: {msg}").into());
    }
    let body: serde_json::Value = resp.json()?;
    let tools = body["tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|t| ToolEntry {
                    name: t["name"].as_str().unwrap_or("?").to_owned(),
                    provider: t["provider"].as_str().unwrap_or("?").to_owned(),
                    description: t["description"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ToolListData { tools })
}

pub fn render_list_text(data: &ToolListData, w: &mut impl Write) -> io::Result<()> {
    if data.tools.is_empty() {
        writeln!(w, "  (no tools available)")?;
        return Ok(());
    }
    for tool in &data.tools {
        if let Some(ref desc) = tool.description {
            writeln!(w, "  {}  [{}]  \u{2014} {desc}", tool.name, tool.provider)?;
        } else {
            writeln!(w, "  {}  [{}]", tool.name, tool.provider)?;
        }
    }
    Ok(())
}

pub fn query_status(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
) -> Result<ToolStatusData, Box<dyn std::error::Error>> {
    let resp = admin_get(config, addr, "/admin/tools/upstreams")?;
    if !resp.status().is_success() {
        let msg = parse_error_message(resp)?;
        return Err(format!("failed to get tool status: {msg}").into());
    }
    let body: serde_json::Value = resp.json()?;
    let servers = body["servers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| ServerEntry {
                    name: s["name"].as_str().unwrap_or("?").to_owned(),
                    tool_count: s["tool_count"].as_u64().unwrap_or(0),
                    filtered: s["filter"].is_object(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ToolStatusData { servers })
}

pub fn render_status_text(data: &ToolStatusData, w: &mut impl Write) -> io::Result<()> {
    if data.servers.is_empty() {
        writeln!(w, "  (no MCP servers configured)")?;
        return Ok(());
    }
    for server in &data.servers {
        let filter_info = if server.filtered { " (filtered)" } else { "" };
        writeln!(
            w,
            "  {}    {} tools{filter_info}",
            server.name, server.tool_count
        )?;
    }
    Ok(())
}

/// Run the `tools list` subcommand.
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

/// Run the `tools status` subcommand.
pub fn run_status(
    config: &bitrouter_config::BitrouterConfig,
    addr: SocketAddr,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = query_status(config, addr)?;
    match output {
        OutputFormat::Text => render_status_text(&data, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &data)?,
    }
    Ok(())
}

/// Run the `tools discover` subcommand — connects to an MCP upstream and
/// outputs YAML tool stanzas for `bitrouter.yaml`.
#[cfg(feature = "mcp")]
pub async fn run_discover(
    config: &bitrouter_config::BitrouterConfig,
    provider_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use bitrouter::providers::mcp::client::config::{McpServerConfig, McpServerTransport};
    use bitrouter::providers::mcp::client::upstream::UpstreamConnection;

    let provider = config
        .providers
        .get(provider_name)
        .ok_or_else(|| format!("provider '{provider_name}' not found in config"))?;

    let url = provider
        .api_base
        .as_deref()
        .ok_or_else(|| format!("provider '{provider_name}' has no api_base configured"))?;

    let mut headers = provider.default_headers.clone().unwrap_or_default();
    if let Some(ref key) = provider.api_key {
        headers
            .entry("Authorization".to_owned())
            .or_insert_with(|| format!("Bearer {key}"));
    }

    let mcp_config = McpServerConfig {
        name: provider_name.to_owned(),
        transport: McpServerTransport::Http {
            url: url.to_owned(),
            headers,
        },
    };

    eprintln!("Connecting to {provider_name} ({url})...");

    let conn = UpstreamConnection::connect(mcp_config, None)
        .await
        .map_err(|e| format!("failed to connect to '{provider_name}': {e}"))?;

    let tools = conn.raw_tools().await;

    if tools.is_empty() {
        eprintln!("No tools discovered from '{provider_name}'.");
        return Ok(());
    }

    eprintln!("Discovered {} tools from '{provider_name}'.", tools.len());
    eprintln!();

    println!("# Discovered from provider \"{provider_name}\"");
    println!("# Paste under the `tools:` section of your bitrouter.yaml");
    println!();
    println!("tools:");

    for tool in &tools {
        println!("  {}:", tool.name);
        println!("    endpoints:");
        println!("      - provider: {provider_name}");
        println!("        tool_id: {}", tool.name);
        if let Some(ref desc) = tool.description {
            let escaped = desc.replace('"', "\\\"");
            println!("    description: \"{escaped}\"");
        }
        if tool.input_schema.is_object() && tool.input_schema.get("properties").is_some() {
            println!(
                "    input_schema: {}",
                serde_json::to_string(&tool.input_schema).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Stub for when MCP feature is disabled.
#[cfg(not(feature = "mcp"))]
pub async fn run_discover(
    _config: &bitrouter_config::BitrouterConfig,
    _provider_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("MCP feature is not enabled — cannot discover tools".into())
}
