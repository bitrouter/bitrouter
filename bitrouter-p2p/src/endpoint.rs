//! Iroh endpoint lifecycle management.
//!
//! Binds an iroh QUIC endpoint from a 32-byte Ed25519 seed and manages
//! the accept loop for inbound P2P connections.

use std::collections::HashSet;
use std::sync::Arc;

use bitrouter_core::errors::BitrouterError;
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, EndpointId, SecretKey};
use tokio::sync::watch;

use crate::frame::ALPN;
use crate::inbound::InboundHandler;

/// Wrapper around an iroh `Endpoint` with BitRouter-specific lifecycle.
pub struct P2pEndpoint {
    endpoint: Endpoint,
}

impl P2pEndpoint {
    /// Construct an iroh endpoint from a 32-byte Ed25519 seed.
    ///
    /// The seed is the same `MasterKeypair` seed used for Solana identity,
    /// ensuring the iroh `EndpointId` shares the same cryptographic root.
    pub async fn from_seed(seed: [u8; 32]) -> Result<Self, BitrouterError> {
        let secret_key = SecretKey::from_bytes(&seed);
        let endpoint = Endpoint::builder(N0)
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| {
                BitrouterError::transport(None, format!("iroh endpoint bind failed: {e}"))
            })?;
        Ok(Self { endpoint })
    }

    /// The local EndpointId (Ed25519 public key).
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Get a reference to the inner iroh Endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Start the inbound accept loop.
    ///
    /// Spawns a background task that accepts QUIC connections from peers
    /// in the allow list and dispatches them through `handler`. The loop
    /// exits when `shutdown_rx` fires or the endpoint is closed.
    pub fn accept(
        &self,
        handler: Arc<InboundHandler>,
        allow_list: Arc<HashSet<EndpointId>>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    conn = endpoint.accept() => {
                        match conn {
                            Some(c) => c,
                            None => break, // endpoint closed
                        }
                    }
                    _ = shutdown_rx.changed() => break,
                };

                // Accept the connection to proceed with the TLS handshake.
                let accepting = match incoming.accept() {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(error = %e, "P2P incoming accept failed");
                        continue;
                    }
                };

                let allow_list = Arc::clone(&allow_list);
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    match accepting.await {
                        Ok(conn) => {
                            // Enforce allow list after handshake completes
                            // (remote_id is only available on Connection).
                            // Empty allow list = refuse all (outbound-only mode).
                            let remote = conn.remote_id();
                            if !allow_list.contains(&remote) {
                                tracing::warn!(
                                    peer = %remote.fmt_short(),
                                    "P2P connection refused: peer not in allow_list",
                                );
                                conn.close(0u32.into(), b"not authorized");
                                return;
                            }

                            if let Err(e) = handler.handle_connection(conn).await {
                                tracing::warn!(
                                    error = %e,
                                    "P2P connection handler error",
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "P2P connection handshake failed",
                            );
                        }
                    }
                });
            }
            tracing::info!("P2P accept loop stopped");
        })
    }

    /// Gracefully close the iroh endpoint.
    pub async fn shutdown(self) {
        self.endpoint.close().await;
        tracing::info!("iroh P2P endpoint closed");
    }
}
