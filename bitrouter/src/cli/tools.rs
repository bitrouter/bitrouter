//! `bitrouter tools` subcommand — inspect MCP tools and servers on a running daemon,
//! and discover tools from MCP upstreams for config authoring.

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

/// Run the `tools discover` subcommand — connects to an MCP upstream and
/// outputs YAML tool stanzas for `bitrouter.yaml`.
#[cfg(feature = "mcp")]
pub async fn run_discover(
    config: &bitrouter_config::BitrouterConfig,
    provider_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_providers::mcp::client::config::{McpServerConfig, McpServerTransport};
    use bitrouter_providers::mcp::client::upstream::UpstreamConnection;

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

    // Output YAML stanzas for pasting into bitrouter.yaml.
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
            // Escape double quotes in YAML string.
            let escaped = desc.replace('"', "\\\"");
            println!("    description: \"{escaped}\"");
        }
        // Include input_schema if it has properties.
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
