use bitrouter_sdk::config::{Config, McpUpstreamProtocol};

#[path = "../../../crates/bitrouter-orchestrator/tests/harness_resources/mcp_fixture.rs"]
mod mcp_fixture;

#[tokio::test]
async fn canonical_mcp_check_uses_harness_discovery_and_preserves_upstream_names()
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
