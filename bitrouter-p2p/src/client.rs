//! Outbound P2P client for tunneling requests to remote BitRouter peers.

use std::pin::Pin;

use bitrouter_core::errors::BitrouterError;
use bytes::Bytes;
use futures_core::Stream;
use iroh::{Endpoint, EndpointAddr};

use crate::frame::{
    ALPN, TunnelRequest, TunnelResponseHeader, read_response_header, write_request,
};

/// Handle to a tunnel response — either complete or streaming.
pub enum TunnelResponse {
    /// Non-streaming: the full body is available.
    Complete {
        status: u16,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
    },
    /// Streaming: SSE bytes arrive via an async stream.
    Streaming {
        status: u16,
        headers: std::collections::HashMap<String, String>,
        body_stream: Pin<Box<dyn Stream<Item = Result<Bytes, BitrouterError>> + Send>>,
    },
}

/// Send a request to a remote BitRouter peer over iroh QUIC and return
/// the response.
///
/// `remote` can be an [`EndpointId`] (discovery-based) or a full
/// [`EndpointAddr`] (with relay URL / direct addresses for faster dialling).
pub async fn send_request(
    endpoint: &Endpoint,
    remote: impl Into<EndpointAddr>,
    request: TunnelRequest,
) -> Result<TunnelResponse, BitrouterError> {
    let remote: EndpointAddr = remote.into();
    let remote_id = remote.id;
    // 1. Connect to the remote peer.
    let conn = endpoint.connect(remote, ALPN).await.map_err(|e| {
        BitrouterError::transport(
            Some("p2p"),
            format!("failed to connect to peer {}: {e}", remote_id.fmt_short()),
        )
    })?;

    // 2. Open a bidirectional stream.
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| {
        BitrouterError::transport(
            Some("p2p"),
            format!(
                "failed to open stream to peer {}: {e}",
                remote_id.fmt_short()
            ),
        )
    })?;

    // 3. Write the request.
    write_request(&mut send, &request).await?;

    // Signal that we're done sending the request.
    send.finish()
        .map_err(|e| BitrouterError::transport(Some("p2p"), format!("P2P send finish: {e}")))?;

    // 4. Read the response header.
    let header: TunnelResponseHeader = read_response_header(&mut recv).await?;

    // 5. Handle non-streaming vs streaming response.
    if header.streaming {
        // Wrap the recv stream as an async byte stream.
        let body_stream = recv_to_byte_stream(recv);
        Ok(TunnelResponse::Streaming {
            status: header.status,
            headers: header.headers,
            body_stream: Box::pin(body_stream),
        })
    } else {
        // Read the complete body frame.
        let body = crate::frame::read_frame(&mut recv).await?;
        Ok(TunnelResponse::Complete {
            status: header.status,
            headers: header.headers,
            body,
        })
    }
}

/// Convert an iroh `RecvStream` into an async `Stream<Item = Result<Bytes>>`
/// by spawning a reader task that pumps chunks into an mpsc channel.
fn recv_to_byte_stream(
    mut recv: iroh::endpoint::RecvStream,
) -> impl Stream<Item = Result<Bytes, BitrouterError>> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        loop {
            let mut buf = vec![0u8; 8192];
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    buf.truncate(n);
                    if tx.send(Ok(Bytes::from(buf))).await.is_err() {
                        break; // receiver dropped
                    }
                }
                Ok(_) => break, // EOF or 0 bytes
                Err(e) => {
                    let _ = tx
                        .send(Err(BitrouterError::transport(
                            Some("p2p"),
                            format!("P2P recv stream error: {e}"),
                        )))
                        .await;
                    break;
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}
