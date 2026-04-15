//! Wire format for tunneling HTTP requests/responses over QUIC streams.
//!
//! Each QUIC bidirectional stream carries one request/response pair.
//! Frames are length-prefixed (4-byte big-endian u32) followed by JSON.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use bitrouter_core::errors::BitrouterError;

/// ALPN protocol identifier for the BitRouter P2P tunnel.
pub const ALPN: &[u8] = b"bitrouter-p2p/1";

/// Maximum frame size: 64 MiB. Prevents unbounded allocations from
/// malformed or malicious peers.
const MAX_FRAME_SIZE: u32 = 64 * 1024 * 1024;

/// A tunneled HTTP request sent from the outbound proxy to the inbound proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRequest {
    /// HTTP method (always "POST" for LLM APIs).
    pub method: String,
    /// Request path, e.g. "/v1/chat/completions".
    pub path: String,
    /// HTTP headers as key-value pairs.
    pub headers: HashMap<String, String>,
    /// Raw JSON request body.
    #[serde(with = "serde_bytes_as_base64")]
    pub body: Vec<u8>,
}

/// Response header sent back from the inbound proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelResponseHeader {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// Whether the response body is SSE-streamed.
    pub streaming: bool,
}

/// Write a length-prefixed frame to a QUIC send stream.
pub async fn write_frame(
    stream: &mut iroh::endpoint::SendStream,
    data: &[u8],
) -> Result<(), BitrouterError> {
    let len = u32::try_from(data.len())
        .map_err(|_| BitrouterError::transport(None, "P2P frame too large to encode"))?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| BitrouterError::transport(None, format!("P2P write length: {e}")))?;
    stream
        .write_all(data)
        .await
        .map_err(|e| BitrouterError::transport(None, format!("P2P write body: {e}")))?;
    Ok(())
}

/// Read a length-prefixed frame from a QUIC receive stream.
pub async fn read_frame(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<Vec<u8>, BitrouterError> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| BitrouterError::transport(None, format!("P2P read length: {e}")))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(BitrouterError::transport(
            None,
            format!("P2P frame size {len} exceeds maximum {MAX_FRAME_SIZE}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| BitrouterError::transport(None, format!("P2P read body: {e}")))?;
    Ok(buf)
}

/// Encode and write a `TunnelRequest` to a QUIC send stream.
pub async fn write_request(
    stream: &mut iroh::endpoint::SendStream,
    req: &TunnelRequest,
) -> Result<(), BitrouterError> {
    let header_json = serde_json::to_vec(req)
        .map_err(|e| BitrouterError::transport(None, format!("P2P request serialize: {e}")))?;
    write_frame(stream, &header_json).await
}

/// Read and decode a `TunnelRequest` from a QUIC receive stream.
pub async fn read_request(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<TunnelRequest, BitrouterError> {
    let data = read_frame(stream).await?;
    serde_json::from_slice(&data)
        .map_err(|e| BitrouterError::transport(None, format!("P2P request deserialize: {e}")))
}

/// Encode and write a `TunnelResponseHeader` to a QUIC send stream.
pub async fn write_response_header(
    stream: &mut iroh::endpoint::SendStream,
    header: &TunnelResponseHeader,
) -> Result<(), BitrouterError> {
    let json = serde_json::to_vec(header).map_err(|e| {
        BitrouterError::transport(None, format!("P2P response header serialize: {e}"))
    })?;
    write_frame(stream, &json).await
}

/// Read and decode a `TunnelResponseHeader` from a QUIC receive stream.
pub async fn read_response_header(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<TunnelResponseHeader, BitrouterError> {
    let data = read_frame(stream).await?;
    serde_json::from_slice(&data).map_err(|e| {
        BitrouterError::transport(None, format!("P2P response header deserialize: {e}"))
    })
}

/// Serde helper: encode `Vec<u8>` as base64 in JSON to avoid huge arrays.
mod serde_bytes_as_base64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_request_roundtrip() {
        let req = TunnelRequest {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            headers: HashMap::from([("content-type".into(), "application/json".into())]),
            body: br#"{"model":"gpt-4o","messages":[]}"#.to_vec(),
        };
        let json = serde_json::to_vec(&req).expect("serialize");
        let decoded: TunnelRequest = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(decoded.path, "/v1/chat/completions");
        assert_eq!(decoded.body, req.body);
    }

    #[test]
    fn tunnel_response_header_roundtrip() {
        let header = TunnelResponseHeader {
            status: 200,
            headers: HashMap::from([("content-type".into(), "text/event-stream".into())]),
            streaming: true,
        };
        let json = serde_json::to_vec(&header).expect("serialize");
        let decoded: TunnelResponseHeader = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(decoded.status, 200);
        assert!(decoded.streaming);
    }
}
