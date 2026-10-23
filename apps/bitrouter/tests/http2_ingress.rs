//! Feature-selected cleartext HTTP/2 on the ordinary application listener.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use bitrouter::gateway::assembly::App;
use bitrouter::gateway::pipeline::executor::MockExecutor;
use bitrouter::gateway::pipeline::routing::StaticRoutingTable;
use tokio::sync::oneshot;

#[tokio::test]
async fn application_listener_preserves_http1_api() -> Result<()> {
    let app = App::builder()
        .skip_auth(true)
        .language_model(|pipeline| {
            pipeline
                .routing_table(Arc::new(StaticRoutingTable::new()))
                .executor(Arc::new(MockExecutor::new(Vec::new())));
        })
        .build()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        app.serve_listener_with_router_wrapper_and_shutdown(listener, |router| router, async {
            let _ = stopped.await;
        })
        .await
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client
        .get(format!("http://{address}/health"))
        .send()
        .await?;
    ensure!(response.status().is_success() && response.version() == http::Version::HTTP_11);
    let expected: serde_json::Value = response.json().await?;
    #[cfg(feature = "http2")]
    {
        let client = reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .timeout(Duration::from_secs(5))
            .build()?;
        let response = client
            .get(format!("http://{address}/health"))
            .send()
            .await?;
        ensure!(response.status().is_success() && response.version() == http::Version::HTTP_2);
        ensure!(response.json::<serde_json::Value>().await? == expected);
        let response = client
            .post(format!("http://{address}/v1/chat/completions"))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        ensure!(response.status().is_client_error() && response.version() == http::Version::HTTP_2);
    }
    #[cfg(not(feature = "http2"))]
    ensure!(expected.is_object());
    drop(client);
    stop.send(())
        .map_err(|_| anyhow::anyhow!("application shutdown channel closed"))?;
    tokio::time::timeout(Duration::from_secs(10), server).await???;
    Ok(())
}
