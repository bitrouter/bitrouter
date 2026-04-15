//! Inbound P2P request handler.
//!
//! Accepts tunneled HTTP requests from QUIC streams and forwards them
//! to the local BitRouter HTTP server via localhost. This reuses the
//! full existing request pipeline (auth, guardrails, routing, streaming).

use std::net::SocketAddr;

use bitrouter_core::errors::BitrouterError;
use iroh::endpoint::Connection;
use tokio_stream::StreamExt;

use crate::frame::{TunnelResponseHeader, read_request, write_frame, write_response_header};

/// Handles inbound P2P connections by forwarding requests to the local
/// BitRouter HTTP server.
pub struct InboundHandler {
    /// The local HTTP server address to forward requests to.
    local_addr: SocketAddr,
    /// HTTP client for localhost forwarding.
    client: reqwest::Client,
}

impl InboundHandler {
    /// Create a new inbound handler that forwards to `local_addr`.
    pub fn new(local_addr: SocketAddr) -> Self {
        // no_proxy() + build() can only fail on invalid TLS config,
        // which is impossible with default settings. Fall through to
        // default client only if the builder is somehow broken.
        let client = match reqwest::Client::builder().no_proxy().build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to build no-proxy HTTP client, using default");
                reqwest::Client::new()
            }
        };
        Self { local_addr, client }
    }

    /// Handle a single inbound QUIC connection.
    ///
    /// Each bidirectional stream on the connection carries one
    /// request/response pair. The connection stays open for multiple
    /// sequential requests (connection reuse).
    pub async fn handle_connection(&self, conn: Connection) -> Result<(), BitrouterError> {
        loop {
            let (send, recv) = match conn.accept_bi().await {
                Ok(pair) => pair,
                Err(iroh::endpoint::ConnectionError::ApplicationClosed(_)) => return Ok(()),
                Err(iroh::endpoint::ConnectionError::LocallyClosed) => return Ok(()),
                Err(e) => {
                    return Err(BitrouterError::transport(
                        None,
                        format!("P2P accept_bi failed: {e}"),
                    ));
                }
            };

            if let Err(e) = self.handle_stream(send, recv).await {
                tracing::warn!(error = %e, "P2P stream handler error");
                // Continue accepting new streams on the same connection.
            }
        }
    }

    /// Handle a single QUIC bidirectional stream: read request, forward
    /// to localhost, write response back.
    async fn handle_stream(
        &self,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), BitrouterError> {
        // 1. Read the tunneled request.
        let tunnel_req = read_request(&mut recv).await?;

        // 2. Build the localhost URL.
        let url = format!("http://{}{}", self.local_addr, tunnel_req.path);

        // 3. Build the forwarded HTTP request.
        let method = tunnel_req
            .method
            .parse::<reqwest::Method>()
            .map_err(|e| BitrouterError::transport(None, format!("invalid HTTP method: {e}")))?;
        let mut builder = self.client.request(method, &url);

        for (key, value) in &tunnel_req.headers {
            // Skip hop-by-hop headers that don't apply to the forwarded request.
            let lower = key.to_lowercase();
            if lower == "host" || lower == "content-length" {
                continue;
            }
            builder = builder.header(key.as_str(), value.as_str());
        }

        builder = builder.body(tunnel_req.body);

        // 4. Send the request to localhost.
        let response = builder.send().await.map_err(|e| {
            BitrouterError::transport(None, format!("P2P localhost forward failed: {e}"))
        })?;

        // 5. Extract response metadata.
        let status = response.status().as_u16();
        let is_streaming = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));

        let mut resp_headers = std::collections::HashMap::new();
        for (name, value) in response.headers() {
            if let Ok(v) = value.to_str() {
                resp_headers.insert(name.as_str().to_owned(), v.to_owned());
            }
        }

        // 6. Write the response header.
        let header = TunnelResponseHeader {
            status,
            headers: resp_headers,
            streaming: is_streaming,
        };
        write_response_header(&mut send, &header).await?;

        // 7. Write the response body.
        if is_streaming {
            self.stream_response_body(&mut send, response).await?;
        } else {
            // Non-streaming: read the entire body and write as a single frame.
            let body = response.bytes().await.map_err(|e| {
                BitrouterError::transport(None, format!("P2P response body read failed: {e}"))
            })?;
            write_frame(&mut send, &body).await?;
        }

        send.finish()
            .map_err(|e| BitrouterError::transport(None, format!("P2P send finish: {e}")))?;
        Ok(())
    }

    /// Stream the response body from localhost to the QUIC send stream.
    ///
    /// For SSE responses, raw bytes are written directly — the client
    /// side receives them exactly as the provider sent them.
    async fn stream_response_body(
        &self,
        send: &mut iroh::endpoint::SendStream,
        response: reqwest::Response,
    ) -> Result<(), BitrouterError> {
        let mut byte_stream = response.bytes_stream();
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| {
                BitrouterError::transport(None, format!("P2P upstream stream error: {e}"))
            })?;
            // Write raw SSE bytes directly to the QUIC stream.
            // No length-prefixing for streaming — the stream close signals EOF.
            send.write_all(&chunk).await.map_err(|e| {
                BitrouterError::transport(None, format!("P2P stream write error: {e}"))
            })?;
        }
        Ok(())
    }
}
