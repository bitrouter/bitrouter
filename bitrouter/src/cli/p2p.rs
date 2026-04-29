use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bitrouter_p2p::direct::{
    DirectConsumer, DirectProvider, DirectRequest, DirectResponse, SolanaChargeConfig,
};
use bitrouter_p2p::node::{P2pConfig, PeerAddr, RelayConfig};
use serde_json::json;

pub async fn run_provider(
    data_dir: PathBuf,
    addr_file: Option<PathBuf>,
    relay_url: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let relay_enabled = relay_url.is_some();
    let provider = DirectProvider::spawn(
        p2p_config(data_dir, relay_url)?,
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
                            "content": format!("bitrouter p2p provider handled {}", request.path)
                        },
                        "finish_reason": "stop"
                    }]
                }),
            )
        }),
    )
    .await?;
    let addr = if relay_enabled {
        match provider.node().relay_node_addr().await {
            Some(addr) => addr,
            None => provider.node().node_addr().await,
        }
    } else {
        provider.node().node_addr().await
    };
    let addr_json = serde_json::to_string_pretty(&addr)?;
    if let Some(path) = addr_file {
        std::fs::write(&path, format!("{addr_json}\n"))?;
        println!("provider_addr_file: {}", path.display());
    }
    println!("{addr_json}");
    println!("p2p provider is running; press Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;
    provider.shutdown().await;
    Ok(())
}

pub async fn run_consumer(
    data_dir: PathBuf,
    provider_addr_file: PathBuf,
    relay_url: Option<String>,
    model: String,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider_addr: PeerAddr =
        serde_json::from_str(&std::fs::read_to_string(provider_addr_file)?)?;
    let consumer = DirectConsumer::spawn(p2p_config(data_dir, relay_url)?).await?;
    let response = consumer
        .request(
            provider_addr,
            &DirectRequest::openai_chat(
                "req-bitrouter-p2p-demo",
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": prompt}]
                }),
            ),
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&response.payload)?);
    consumer.shutdown().await;
    Ok(())
}

pub fn validate_solana_charge(
    network: String,
    recipient: String,
    currency: Option<String>,
    amount_base_units: String,
    decimals: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = SolanaChargeConfig {
        network,
        recipient,
        currency,
        amount_base_units,
        decimals,
    };
    config.validate()?;
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

fn p2p_config(
    data_dir: PathBuf,
    relay_url: Option<String>,
) -> Result<P2pConfig, Box<dyn std::error::Error>> {
    let relay = match relay_url {
        Some(url) => RelayConfig::Custom(vec![url.parse()?]),
        None => RelayConfig::Disabled,
    };
    Ok(P2pConfig {
        data_dir,
        relay,
        publish_discovery: false,
        connect_timeout: Duration::from_secs(30),
        ..P2pConfig::default()
    })
}
