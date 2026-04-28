use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::node::{P2pConfig, P2pError, P2pNode};
use crate::primitives::types::ALPN_DIRECT;

const MAX_DIRECT_FRAME_BYTES: usize = 8 * 1024 * 1024;
const TYPE_DIRECT_REQUEST: &str = "bitrouter/direct/request/0";
const TYPE_DIRECT_RESPONSE: &str = "bitrouter/direct/response/0";

#[derive(Debug, Error)]
pub enum DirectError {
    #[error("p2p node: {0}")]
    P2p(#[from] P2pError),
    #[error("inbound receiver already taken")]
    InboundTaken,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("unexpected direct message type: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRequest {
    #[serde(rename = "type")]
    pub type_id: String,
    pub request_id: String,
    pub path: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment: Option<SolanaChargePayment>,
}

impl DirectRequest {
    pub fn openai_chat(request_id: impl Into<String>, payload: Value) -> Self {
        Self {
            type_id: TYPE_DIRECT_REQUEST.to_owned(),
            request_id: request_id.into(),
            path: "/v1/chat/completions".to_owned(),
            payload,
            payment: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectResponse {
    #[serde(rename = "type")]
    pub type_id: String,
    pub request_id: String,
    pub status: u16,
    pub payload: Value,
}

impl DirectResponse {
    pub fn ok(request_id: impl Into<String>, payload: Value) -> Self {
        Self {
            type_id: TYPE_DIRECT_RESPONSE.to_owned(),
            request_id: request_id.into(),
            status: 200,
            payload,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SolanaChargeConfig {
    pub network: String,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub amount_base_units: String,
    pub decimals: u8,
}

impl SolanaChargeConfig {
    pub fn validate(&self) -> Result<(), DirectError> {
        if self.network.trim().is_empty()
            || self.recipient.trim().is_empty()
            || self.amount_base_units.trim().is_empty()
        {
            return Err(DirectError::TypeMismatch {
                expected: "non-empty solana charge config".to_owned(),
                actual: "empty field".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolanaChargePayment {
    pub intent: String,
    pub method: String,
    pub config: SolanaChargeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_signature: Option<String>,
}

pub type DirectHandler = Arc<dyn Fn(DirectRequest) -> DirectResponse + Send + Sync + 'static>;

pub struct DirectProvider {
    node: P2pNode,
    accept_task: JoinHandle<()>,
}

impl DirectProvider {
    pub async fn spawn(config: P2pConfig, handler: DirectHandler) -> Result<Self, DirectError> {
        let node = P2pNode::spawn(config).await?;
        let mut inbound = node.inbound().await.ok_or(DirectError::InboundTaken)?;
        let accept_task = tokio::spawn(async move {
            while let Some(inbound) = inbound.recv().await {
                if let Some(alpn) = inbound.conn.alpn()
                    && alpn != ALPN_DIRECT.as_bytes()
                {
                    inbound.conn.close();
                    continue;
                }
                let handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_connection(inbound.conn, handler).await {
                        warn!(error = %err, "direct provider connection failed");
                    }
                });
            }
        });
        Ok(Self { node, accept_task })
    }

    pub fn node(&self) -> &P2pNode {
        &self.node
    }

    pub async fn shutdown(self) {
        self.accept_task.abort();
        let _ = self.accept_task.await;
        self.node.shutdown().await;
    }
}

pub struct DirectConsumer {
    node: P2pNode,
}

impl DirectConsumer {
    pub async fn spawn(config: P2pConfig) -> Result<Self, DirectError> {
        Ok(Self {
            node: P2pNode::spawn(config).await?,
        })
    }

    pub async fn request(
        &self,
        provider: iroh::NodeAddr,
        request: &DirectRequest,
    ) -> Result<DirectResponse, DirectError> {
        let conn = self.node.connect_addr(provider).await.map_err(|err| {
            std::io::Error::other(format!(
                "direct connection failed: {} ({})",
                err,
                err.kind()
            ))
        })?;
        tracing::debug!("direct consumer connected");
        let (mut send, mut recv) = conn.open_bi().await?;
        tracing::debug!("direct consumer opened bidirectional stream");
        write_frame(&mut send, request).await?;
        tracing::debug!("direct consumer wrote request frame");
        let _ = send.finish();
        let response: DirectResponse = read_frame(&mut recv).await?;
        tracing::debug!("direct consumer read response frame");
        if response.type_id != TYPE_DIRECT_RESPONSE {
            return Err(DirectError::TypeMismatch {
                expected: TYPE_DIRECT_RESPONSE.to_owned(),
                actual: response.type_id,
            });
        }
        Ok(response)
    }

    pub async fn shutdown(self) {
        self.node.shutdown().await;
    }
}

async fn serve_connection(
    conn: crate::node::P2pConnection,
    handler: DirectHandler,
) -> Result<(), DirectError> {
    loop {
        tracing::debug!("direct provider waiting for bidirectional stream");
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(streams) => streams,
            Err(err)
                if err.kind() == std::io::ErrorKind::NotConnected
                    || err.to_string().contains("closed by peer") =>
            {
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        tracing::debug!("direct provider accepted bidirectional stream");
        let request: DirectRequest = read_frame(&mut recv).await?;
        tracing::debug!("direct provider read request frame");
        if request.type_id != TYPE_DIRECT_REQUEST {
            return Err(DirectError::TypeMismatch {
                expected: TYPE_DIRECT_REQUEST.to_owned(),
                actual: request.type_id,
            });
        }
        let response = handler(request);
        write_frame(&mut send, &response).await?;
        tracing::debug!("direct provider wrote response frame");
        let _ = send.finish();
    }
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), DirectError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_DIRECT_FRAME_BYTES {
        return Err(DirectError::FrameTooLarge(bytes.len()));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, DirectError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let len = reader.read_u32().await? as usize;
    if len > MAX_DIRECT_FRAME_BYTES {
        return Err(DirectError::FrameTooLarge(len));
    }
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
