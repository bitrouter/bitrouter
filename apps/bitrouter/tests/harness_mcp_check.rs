use bitrouter::config::Config;
use bitrouter::config::McpUpstreamProtocol;
use bitrouter::gateway::mcp::transport::{McpServerConfig, McpTransport};

#[path = "harness_resources/mcp_fixture.rs"]
mod mcp_fixture;

#[cfg(unix)]
#[path = "harness_resources/mcp_process.rs"]
mod mcp_process;

#[tokio::test]
async fn canonical_mcp_check_uses_bounded_discovery_and_preserves_upstream_names()
-> Result<(), Box<dyn std::error::Error>> {
    let upstream = mcp_fixture::server(false).await;
    let mut config = Config::default();
    config.mcp.upstream_protocol = McpUpstreamProtocol::Latest;
    config
        .mcp_servers
        .insert("fixture".into(), mcp_fixture::configuration(&upstream));
    let rows = bitrouter::tools::check(&config, Some("fixture")).await?;
    assert_eq!(rows.len(), 1);
    let tools = rows[0]
        .outcome
        .as_ref()
        .map_err(|error| error.to_string())?;
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["echo", "other"]
    );
    let requests = upstream.received_requests().await.ok_or("requests")?;
    let messages = requests
        .iter()
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body))
        .collect::<Result<Vec<_>, _>>()?;
    let initialize = messages
        .iter()
        .find(|message| message["method"] == "initialize")
        .ok_or("initialize")?;
    assert_eq!(
        initialize["params"]["clientInfo"]["name"],
        "bitrouter-harness"
    );
    assert_eq!(
        initialize["params"]["clientInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "tools/call")
    );
    Ok(())
}

#[tokio::test]
async fn default_mcp_check_uses_modern_discovery_without_legacy_initialize()
-> Result<(), Box<dyn std::error::Error>> {
    let upstream = mcp_fixture::server(false).await;
    let mut config = Config::default();
    config
        .mcp_servers
        .insert("fixture".into(), mcp_fixture::configuration(&upstream));
    let rows = bitrouter::tools::check(&config, Some("fixture")).await?;
    assert_eq!(
        rows[0]
            .outcome
            .as_ref()
            .map_err(|error| error.to_string())?
            .len(),
        2
    );
    let requests = upstream.received_requests().await.ok_or("requests")?;
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .map(|value| value["method"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(methods.iter().any(|method| method == "server/discover"));
    assert!(!methods.iter().any(|method| method == "initialize"));
    Ok(())
}

#[tokio::test]
async fn mcp_check_rejects_repeated_pagination_without_harness()
-> Result<(), Box<dyn std::error::Error>> {
    let upstream = mcp_fixture::server(true).await;
    let mut config = Config::default();
    config.mcp.upstream_protocol = McpUpstreamProtocol::Latest;
    config
        .mcp_servers
        .insert("fixture".into(), mcp_fixture::configuration(&upstream));
    let rows = bitrouter::tools::check(&config, None).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].outcome.as_ref().err().map(String::as_str),
        Some("MCP tools/list repeated or empty pagination cursor")
    );
    Ok(())
}

#[tokio::test]
async fn shared_discovery_preserves_prefix_headers_and_server_instructions()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter::gateway::mcp::client::BoundedMcpClient;
    use rmcp::model::ProtocolVersion;

    let upstream = mcp_fixture::server(false).await;
    let mut server = mcp_fixture::configuration(&upstream);
    server.tool_prefix = Some("custom_".into());
    if let McpTransport::Http { headers, .. } = &mut server.transport {
        headers.insert("x-fixture".into(), "bounded-client".into());
    }
    let workspace = tempfile::TempDir::new()?;
    let mut client =
        BoundedMcpClient::connect(workspace.path(), &[server], ProtocolVersion::LATEST).await?;
    client.validate_catalog()?;
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["custom_echo", "custom_other"]
    );
    assert_eq!(client.instructions().len(), 1);
    assert_eq!(client.instructions()[0].server, "fixture");
    assert_eq!(client.instructions()[0].text, "Use echo with text.");
    assert_eq!(client.tool_peer("custom_echo")?.0, "echo");
    client.shutdown().await?;
    assert!(client.tools().is_empty());
    assert!(client.instructions().is_empty());
    assert!(client.tool_peer("custom_echo").is_err());
    let requests = upstream.received_requests().await.ok_or("requests")?;
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| {
        request
            .headers
            .get("x-fixture")
            .is_some_and(|value| value == "bounded-client")
    }));
    Ok(())
}

#[tokio::test]
async fn shared_discovery_retains_catalog_and_instruction_limits()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter::gateway::mcp::client::BoundedMcpClient;
    use rmcp::model::ProtocolVersion;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    // The complete frozen declaration, including local annotations, historically
    // defined the byte budget. Exercise the exact boundary through MCP-only I/O.
    let base = json!({"name":"fixture__echo","description":"","parameters":{"type":"object"},"effect":"unknown","approval_required":true});
    let remaining = 1024 * 1024 - serde_json::to_vec(&base)?.len();
    let cases = [
        (
            json!([{"name":"echo","description":"x".repeat(remaining),"inputSchema":{"type":"object"}}]),
            String::new(),
            None,
        ),
        (
            json!([{"name":"echo","description":"x".repeat(remaining + 1),"inputSchema":{"type":"object"}}]),
            String::new(),
            Some("MCP tool catalog exceeds 1 MiB"),
        ),
        (
            json!(
                (0..257)
                    .map(|i| json!({"name":format!("tool_{i}"),"inputSchema":{"type":"object"}}))
                    .collect::<Vec<_>>()
            ),
            String::new(),
            Some("MCP catalog exceeds 256 tools"),
        ),
        (
            json!([]),
            "x".repeat(64 * 1024 + 1),
            Some("MCP server instructions exceed 64 KiB"),
        ),
        (
            json!([{"name":"echo","inputSchema":{"type":"array"}}]),
            String::new(),
            Some("MCP tool input schema must describe an object"),
        ),
    ];
    let workspace = tempfile::TempDir::new()?;
    for (tools, instructions, expected) in cases {
        let upstream = MockServer::start().await;
        Mock::given(method("POST")).and(path("/mcp"))
            .respond_with(move |request: &Request| {
                let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
                let Some(id) = body.get("id") else { return ResponseTemplate::new(202); };
                let result = match body["method"].as_str() {
                    Some("initialize") => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"},"instructions":instructions}),
                    Some("tools/list") => json!({"tools":tools}),
                    _ => return ResponseTemplate::new(400),
                };
                ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":id,"result":result}))
            }).mount(&upstream).await;
        match BoundedMcpClient::connect(
            workspace.path(),
            &[mcp_fixture::configuration(&upstream)],
            ProtocolVersion::LATEST,
        )
        .await
        {
            Ok(mut client) => {
                client.shutdown().await?;
                assert_eq!(expected, None);
            }
            Err(error) => {
                assert_eq!(Some(error.message.as_str()), expected);
                assert!(!error.cleanup_unknown, "{error}");
            }
        }
    }
    Ok(())
}
