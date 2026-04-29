use std::sync::Arc;
use std::time::Duration;

use bitrouter_p2p::direct::{
    DirectConsumer, DirectProvider, DirectRequest, DirectResponse, SolanaChargeConfig,
};
use bitrouter_p2p::node::{P2pConfig, RelayConfig};
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("bitrouter_p2p=debug")
        .with_test_writer()
        .try_init();
}

fn local_config(dir: &TempDir) -> P2pConfig {
    P2pConfig {
        data_dir: dir.path().to_path_buf(),
        relay: RelayConfig::Disabled,
        publish_discovery: false,
        connect_timeout: Duration::from_secs(5),
        ..P2pConfig::default()
    }
}

#[tokio::test]
async fn provider_and_consumer_exchange_direct_request() -> TestResult {
    init_logging();
    let provider_dir = tempfile::tempdir()?;
    let consumer_dir = tempfile::tempdir()?;
    let provider = DirectProvider::spawn(
        local_config(&provider_dir),
        Arc::new(|request| {
            DirectResponse::ok(
                request.request_id,
                json!({
                    "id": "chatcmpl-local-p2p",
                    "object": "chat.completion",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": format!("p2p provider handled {}", request.path)
                        },
                        "finish_reason": "stop"
                    }]
                }),
            )
        }),
    )
    .await?;
    let provider_addr = provider.node().node_addr().await;

    let consumer = DirectConsumer::spawn(local_config(&consumer_dir)).await?;
    let request = DirectRequest::openai_chat(
        "req-local-1",
        json!({
            "model": "local-p2p-demo",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    );

    let response = timeout(
        Duration::from_secs(10),
        consumer.request(provider_addr, &request),
    )
    .await??;

    assert_eq!(response.status, 200);
    assert_eq!(response.request_id, "req-local-1");
    assert_eq!(
        response.payload["choices"][0]["message"]["content"],
        "p2p provider handled /v1/chat/completions"
    );

    consumer.shutdown().await;
    provider.shutdown().await;
    Ok(())
}

#[test]
fn solana_charge_fixed_price_config_validates_required_fields() -> TestResult {
    let config = SolanaChargeConfig {
        network: "devnet".to_owned(),
        recipient: "9xAXssX9j7vuK99c7cFwqbixzL3bFrzPy9PUhCtDPAYJ".to_owned(),
        currency: None,
        amount_base_units: "1000".to_owned(),
        decimals: 9,
    };
    config.validate()?;

    let invalid = SolanaChargeConfig {
        recipient: String::new(),
        ..config
    };
    assert!(invalid.validate().is_err());
    Ok(())
}
