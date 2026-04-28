use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream};
use iroh::{Endpoint, NodeAddr, NodeId, RelayMode, RelayUrl, SecretKey, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::identity_store::{IdentityStoreError, load_or_create_secret_key};
use crate::primitives::types::ALPN_DIRECT;

#[derive(Debug, Clone)]
pub struct P2pConfig {
    pub data_dir: PathBuf,
    pub relay: RelayConfig,
    pub publish_discovery: bool,
    pub connect_timeout: Duration,
    pub alpn: Cow<'static, [u8]>,
    pub accept_alpns: Vec<Cow<'static, [u8]>>,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./bitrouter-p2p-data"),
            relay: RelayConfig::default(),
            publish_discovery: true,
            connect_timeout: Duration::from_secs(30),
            alpn: Cow::Borrowed(ALPN_DIRECT.as_bytes()),
            accept_alpns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum RelayConfig {
    #[default]
    N0Default,
    Custom(Vec<RelayUrl>),
    Disabled,
}

#[derive(Debug, Error)]
pub enum P2pError {
    #[error("identity: {0}")]
    Identity(#[from] IdentityStoreError),
    #[error("endpoint bind failed: {0}")]
    Bind(String),
    #[error("endpoint closed")]
    Closed,
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("dial timed out")]
    Timeout,
    #[error("peer not found via discovery")]
    DiscoveryFailed,
    #[error("relay unreachable")]
    RelayUnreachable,
    #[error("handshake failed: {0}")]
    HandshakeFailed(String),
    #[error("local node is shut down")]
    LocalShutdown,
}

impl ConnectError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::DiscoveryFailed => "discovery_failed",
            Self::RelayUnreachable => "relay_unreachable",
            Self::HandshakeFailed(_) => "handshake_failed",
            Self::LocalShutdown => "local_shutdown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct P2pConnection {
    inner: Connection,
}

impl P2pConnection {
    fn new(inner: Connection) -> Self {
        Self { inner }
    }

    pub fn remote(&self) -> Result<NodeId, String> {
        self.inner.remote_node_id().map_err(|err| format!("{err}"))
    }

    pub fn alpn(&self) -> Option<Vec<u8>> {
        self.inner.alpn()
    }

    pub async fn open_bi(&self) -> std::io::Result<(SendStream, RecvStream)> {
        self.inner
            .open_bi()
            .await
            .map_err(|err| std::io::Error::other(format!("open_bi: {err}")))
    }

    pub async fn accept_bi(&self) -> std::io::Result<(SendStream, RecvStream)> {
        self.inner
            .accept_bi()
            .await
            .map_err(|err| std::io::Error::other(format!("accept_bi: {err}")))
    }

    pub fn close(&self) {
        self.inner.close(0u32.into(), b"bye");
    }
}

#[derive(Debug)]
pub struct InboundConnection {
    pub conn: P2pConnection,
    pub remote: NodeId,
}

#[derive(Clone)]
pub struct P2pNode {
    endpoint: Endpoint,
    connect_timeout: Duration,
    alpn: Cow<'static, [u8]>,
    inbound_rx: Arc<tokio::sync::Mutex<Option<mpsc::Receiver<InboundConnection>>>>,
    _accept_task: Arc<AcceptTask>,
}

struct AcceptTask(JoinHandle<()>);

impl Drop for AcceptTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl P2pNode {
    pub async fn spawn(config: P2pConfig) -> Result<Self, P2pError> {
        let secret_key = load_or_create_secret_key(&config.data_dir)?;
        Self::spawn_with_secret(config, secret_key).await
    }

    pub async fn spawn_with_secret(
        config: P2pConfig,
        secret_key: SecretKey,
    ) -> Result<Self, P2pError> {
        let alpn = config.alpn.clone();
        let endpoint = build_endpoint(&config, secret_key).await?;
        let endpoint_id = endpoint.node_id();
        info!(%endpoint_id, alpn = %String::from_utf8_lossy(&alpn), "p2p endpoint bound");

        let (tx, rx) = mpsc::channel::<InboundConnection>(64);
        let accept_endpoint = endpoint.clone();
        let handle = tokio::spawn(async move {
            accept_loop(accept_endpoint, tx).await;
        });

        Ok(Self {
            endpoint,
            connect_timeout: config.connect_timeout,
            alpn,
            inbound_rx: Arc::new(tokio::sync::Mutex::new(Some(rx))),
            _accept_task: Arc::new(AcceptTask(handle)),
        })
    }

    pub fn endpoint_id(&self) -> NodeId {
        self.endpoint.node_id()
    }

    pub async fn node_addr(&self) -> NodeAddr {
        self.endpoint.node_addr().initialized().await
    }

    pub async fn connect_addr(&self, addr: NodeAddr) -> Result<P2pConnection, ConnectError> {
        self.connect_addr_with_alpn(addr, &self.alpn).await
    }

    pub async fn connect_addr_with_alpn(
        &self,
        addr: NodeAddr,
        alpn: &[u8],
    ) -> Result<P2pConnection, ConnectError> {
        let dial = self.endpoint.connect(addr, alpn);
        match timeout(self.connect_timeout, dial).await {
            Err(_) => Err(ConnectError::Timeout),
            Ok(Ok(conn)) => Ok(P2pConnection::new(conn)),
            Ok(Err(err)) => {
                let message = format!("{err:?}");
                if message.contains("no addresses") || message.contains("Discovery") {
                    Err(ConnectError::DiscoveryFailed)
                } else if message.contains("relay") {
                    Err(ConnectError::RelayUnreachable)
                } else if message.contains("closed") {
                    Err(ConnectError::LocalShutdown)
                } else {
                    Err(ConnectError::HandshakeFailed(message))
                }
            }
        }
    }

    pub async fn inbound(&self) -> Option<mpsc::Receiver<InboundConnection>> {
        self.inbound_rx.lock().await.take()
    }

    pub async fn shutdown(self) {
        self.endpoint.close().await;
    }
}

async fn build_endpoint(config: &P2pConfig, secret_key: SecretKey) -> Result<Endpoint, P2pError> {
    let mut builder = match &config.relay {
        RelayConfig::Disabled => Endpoint::builder().relay_mode(RelayMode::Disabled),
        RelayConfig::N0Default => Endpoint::builder().relay_mode(RelayMode::Default),
        RelayConfig::Custom(_) => Endpoint::builder(),
    };

    let mut alpns = vec![config.alpn.to_vec()];
    for alpn in &config.accept_alpns {
        let alpn = alpn.to_vec();
        if !alpns.contains(&alpn) {
            alpns.push(alpn);
        }
    }

    builder = builder.secret_key(secret_key).alpns(alpns);

    if let RelayConfig::Custom(urls) = &config.relay {
        let relay_map: iroh::RelayMap = urls.iter().cloned().collect::<iroh::RelayMap>();
        builder = builder.relay_mode(RelayMode::Custom(relay_map));
    }

    if !config.publish_discovery {
        builder = builder.clear_discovery();
        debug!("address lookup cleared for local explicit-address P2P node");
    }

    builder
        .bind()
        .await
        .map_err(|err| P2pError::Bind(format!("{err:?}")))
}

async fn accept_loop(endpoint: Endpoint, tx: mpsc::Sender<InboundConnection>) {
    loop {
        let incoming: Incoming = match endpoint.accept().await {
            Some(incoming) => incoming,
            None => {
                info!("accept loop closed");
                return;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            match incoming.accept() {
                Ok(connecting) => match connecting.await {
                    Ok(conn) => {
                        debug!("accepted inbound p2p connection");
                        let remote = match conn.remote_node_id() {
                            Ok(remote) => remote,
                            Err(err) => {
                                warn!(error = %err, "inbound connection missing remote id");
                                return;
                            }
                        };
                        if tx
                            .send(InboundConnection {
                                conn: P2pConnection::new(conn),
                                remote,
                            })
                            .await
                            .is_err()
                        {
                            debug!("inbound receiver dropped");
                        }
                    }
                    Err(err) => warn!(error = %err, "inbound handshake failed"),
                },
                Err(err) => warn!(error = %err, "inbound accept failed"),
            }
        });
    }
}
