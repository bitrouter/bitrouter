//! OS-local transport and stdio bridge for orchestrator-owned native ACP.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_orchestrator::acp::native::NativeAcpServer;
use bitrouter_orchestrator::agent::AgentConfig;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::daemon::transport;

const BRIDGE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Setup {
    version: u32,
    model: String,
    read_only: bool,
    turn_timeout: Option<u64>,
}

pub fn socket_path(control: &Path) -> PathBuf {
    control.with_extension("acp.sock")
}

/// A bridge owns protocol bytes only; EOF never sends business cancellation.
pub async fn bridge(
    source: &crate::paths::ConfigSource,
    config: &bitrouter_sdk::config::Config,
    routing: &crate::acp_cli::RoutingOptions,
    read_only: bool,
    turn_timeout: Option<u64>,
) -> Result<()> {
    anyhow::ensure!(
        !routing.direct && routing.base_url.is_none(),
        "--direct and --base-url apply to external ACP agents; native ACP uses the local daemon"
    );
    let model = routing
        .model
        .clone()
        .or_else(|| config.chat.model.clone())
        .filter(|model| !model.trim().is_empty())
        .context("native ACP requires --model or chat.model in configuration")?;
    if let Some(seconds) = turn_timeout {
        anyhow::ensure!(
            (1..=86400).contains(&seconds),
            "--turn-timeout must be 1-86400 seconds"
        );
    }
    let control = crate::daemon::socket_path_for(source, config);
    if routing.no_start {
        anyhow::ensure!(
            crate::daemon::probe_status(&control).await?.is_some(),
            "local daemon is unavailable and --no-start was selected"
        );
    } else {
        crate::agent_local::connect_or_start(source, &control).await?;
    }
    let mut stream = transport::connect(&socket_path(&control))
        .await
        .context("native ACP endpoint unavailable; explicitly restart an older daemon")?;
    let mut setup = serde_json::to_vec(&Setup {
        version: BRIDGE_VERSION,
        model,
        read_only,
        turn_timeout,
    })?;
    setup.push(b'\n');
    stream.write_all(&setup).await?;
    stream.flush().await?;
    let (mut daemon_read, mut daemon_write) = tokio::io::split(stream);
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut input, &mut daemon_write) => { result?; daemon_write.shutdown().await?; },
        result = tokio::io::copy(&mut daemon_read, &mut output) => { result?; output.flush().await?; },
    }
    Ok(())
}

pub(crate) async fn serve(
    mut listener: transport::ControlListener,
    server: NativeAcpServer,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut peers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(result) = peers.join_next(), if !peers.is_empty() => {
                if let Ok(Err(error)) = result { tracing::debug!(%error, "native ACP client disconnected"); }
            }
            stream = listener.accept() => {
                let stream = stream?;
                if peers.len() >= 64 { drop(stream); continue; }
                let server = server.clone();
                peers.spawn(async move { serve_peer(stream, server).await });
            }
        }
    }
    peers.abort_all();
    while peers.join_next().await.is_some() {}
    Ok(())
}

async fn serve_peer<S>(stream: S, server: NativeAcpServer) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let setup = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            let byte = read.read_u8().await?;
            if byte == b'\n' {
                break;
            }
            anyhow::ensure!(bytes.len() < 4096, "native ACP setup exceeds byte bound");
            bytes.push(byte);
        }
        serde_json::from_slice::<Setup>(&bytes).map_err(anyhow::Error::from)
    })
    .await??;
    anyhow::ensure!(
        setup.version == BRIDGE_VERSION,
        "native ACP bridge version mismatch"
    );
    let mut agent = AgentConfig::fixed(setup.model, None);
    if setup.read_only {
        agent = agent.read_only();
    }
    if let Some(seconds) = setup.turn_timeout {
        anyhow::ensure!(
            (1..=86400).contains(&seconds),
            "invalid native Turn deadline"
        );
        agent.max_duration = std::time::Duration::from_secs(seconds);
    }
    server
        .with_agent_config(agent)
        .connect(read, write)
        .await
        .map_err(anyhow::Error::from)
}
