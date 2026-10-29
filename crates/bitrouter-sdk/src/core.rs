//! Bounded transport for the existing managed core protocol.
//!
//! Persistence, tool execution and checkpoint acknowledgements belong to the
//! caller. A closed transport is not a completed run or proof of cancellation.

use bitrouter_ai::core::protocol::{BETA, ClientMessage, ServerMessage};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use reqwest::{RequestBuilder, StatusCode, Version, header};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{
    Error as SocketError, Message,
    handshake::{client::generate_key, derive_accept_key},
    protocol::{Role, WebSocketConfig},
};

use crate::error::{Error, Result, ServiceFailure};
use crate::transport;

mod io;

#[cfg(test)]
mod tests;

use io::Transport;

type Socket = WebSocketStream<Transport>;

/// Per-message limits apply to both inbound frames/messages and outbound JSON.
/// They do not replace the service's negotiated execution or checkpoint limits.
#[derive(Clone, Debug)]
pub struct ChannelOptions {
    pub max_message_bytes: usize,
}

impl Default for ChannelOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 16 * 1024 * 1024,
        }
    }
}

/// An authenticated transport, before the caller's explicit bind/restore.
/// Split it to send ACKs or steering while a receiver awaits service events.
pub struct CoreChannel {
    sender: CoreSender,
    receiver: CoreReceiver,
}

/// The sending half of one channel. Failed or cancelled writes invalidate both
/// halves. Bytes already dispatched may still execute; commands are not retried.
pub struct CoreSender {
    inner: SplitSink<Socket, Message>,
    max_message_bytes: usize,
    transport: Transport,
}

/// The receiving half. Messages are data, never permission to commit or execute
/// without the harness's durable ownership and acknowledgement protocol.
pub struct CoreReceiver {
    inner: SplitStream<Socket>,
    transport: Transport,
    ended: bool,
    closing: bool,
}

impl std::fmt::Debug for CoreChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreChannel").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CoreSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreSender")
            .field("max_message_bytes", &self.max_message_bytes)
            .field("write_closed", &self.transport.write_closed())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CoreReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreReceiver")
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl CoreChannel {
    pub fn split(self) -> (CoreSender, CoreReceiver) {
        (self.sender, self.receiver)
    }

    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        self.sender.send(message).await
    }

    /// `None` means the channel ended, not that a run completed or was cancelled.
    pub async fn receive(&mut self) -> Result<Option<ServerMessage>> {
        self.receiver.receive().await
    }

    /// Initiate transport closure. Continue receiving to finish the handshake.
    /// This does not send a run cancellation or release durable ownership.
    pub async fn close(&mut self) -> Result<()> {
        self.sender.close().await
    }
}

impl CoreSender {
    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        if self.transport.write_closed() {
            return Err(Error::CoreChannel("channel is closed for sending"));
        }
        let text = encode(message, self.max_message_bytes)?;
        let guard = self.transport.invalidate_on_drop();
        self.inner
            .send(Message::Text(text.into()))
            .await
            .map_err(socket_error)?;
        guard.disarm();
        Ok(())
    }

    /// Initiate closing; the receiver drives the peer's close acknowledgement.
    /// Closing or dropping the channel never sends a cancellation command.
    pub async fn close(&mut self) -> Result<()> {
        if self.transport.invalidated() {
            return Err(Error::CoreChannel("channel transport is invalidated"));
        }
        self.transport.close_writes();
        let guard = self.transport.invalidate_on_drop();
        match self.inner.close().await {
            Ok(()) | Err(SocketError::ConnectionClosed | SocketError::AlreadyClosed) => {
                guard.disarm();
                Ok(())
            }
            Err(error) => Err(socket_error(error)),
        }
    }
}

impl CoreReceiver {
    /// Receive one public message. Transport/protocol failures end this reader;
    /// subsequent calls return `None`. No failure or EOF authorizes replay.
    pub async fn receive(&mut self) -> Result<Option<ServerMessage>> {
        if self.ended {
            return Ok(None);
        }
        while let Some(message) = self.inner.next().await {
            let result = match message {
                Ok(Message::Text(text)) => serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|_| Error::CoreChannel("invalid service message")),
                // Reading again drives tungstenite's automatic pong/close
                // reply. Framing and masking follow RFC 6455 sections 5–7.
                // https://www.rfc-editor.org/rfc/rfc6455.html#section-5
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => {
                    self.closing = true;
                    self.transport.close_writes();
                    continue;
                }
                Ok(_) => Err(Error::CoreChannel("expected a text protocol message")),
                Err(SocketError::ConnectionClosed) if self.closing => break,
                Err(error) => Err(socket_error(error)),
            };
            if result.is_err() {
                self.finish();
            }
            return result;
        }
        self.finish();
        Ok(None)
    }

    fn finish(&mut self) {
        self.ended = true;
        self.transport.invalidate();
    }
}

pub(crate) async fn connect(
    request: RequestBuilder,
    options: ChannelOptions,
    error_body_limit: usize,
) -> Result<CoreChannel> {
    let write_limit = options
        .max_message_bytes
        .checked_add(14)
        .filter(|_| options.max_message_bytes > 0)
        .ok_or(Error::Options("invalid core message byte limit"))?;
    let config = WebSocketConfig::default()
        .read_buffer_size(options.max_message_bytes.min(128 * 1024))
        .write_buffer_size(0)
        .max_write_buffer_size(write_limit)
        .max_message_size(Some(options.max_message_bytes))
        .max_frame_size(Some(options.max_message_bytes));
    let key = generate_key();
    let accept = derive_accept_key(key.as_bytes());
    // Keep the configured HTTP client's TLS, proxy, auth, redirect and retry
    // policy. Validate RFC 6455's client handshake before upgrading the socket.
    // https://www.rfc-editor.org/rfc/rfc6455.html#section-4.1
    // https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.upgrade
    let response = request
        .version(Version::HTTP_11)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, key)
        .header("BitRouter-Beta", BETA)
        .send()
        .await
        .map_err(Error::transport)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(Error::Core(
            transport::service_failure(response, error_body_limit).await,
        ));
    }
    if !valid_upgrade(response.headers(), &accept) {
        return Err(Error::Core(Box::new(ServiceFailure {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: None,
            body_error: Some(Error::CoreChannel("invalid WebSocket upgrade response")),
        })));
    }
    let upgraded = response.upgrade().await.map_err(Error::transport)?;
    Ok(channel(upgraded, config, options.max_message_bytes).await)
}

async fn channel(
    upgraded: reqwest::Upgraded,
    config: WebSocketConfig,
    max_message_bytes: usize,
) -> CoreChannel {
    let transport = Transport::new(upgraded);
    let socket =
        WebSocketStream::from_raw_socket(transport.clone(), Role::Client, Some(config)).await;
    let (sender, receiver) = socket.split();
    CoreChannel {
        sender: CoreSender {
            inner: sender,
            max_message_bytes,
            transport: transport.clone(),
        },
        receiver: CoreReceiver {
            inner: receiver,
            transport,
            ended: false,
            closing: false,
        },
    }
}

fn valid_upgrade(headers: &header::HeaderMap, accept: &str) -> bool {
    fn single(headers: &header::HeaderMap, name: header::HeaderName) -> Option<&str> {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        if values.next().is_some() {
            return None;
        }
        Some(value)
    }
    single(headers, header::UPGRADE).is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && single(headers, header::SEC_WEBSOCKET_ACCEPT) == Some(accept)
        && headers.get_all(header::CONNECTION).iter().any(|value| {
            value.to_str().is_ok_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
        })
        && !headers.contains_key(header::SEC_WEBSOCKET_EXTENSIONS)
        && !headers.contains_key(header::SEC_WEBSOCKET_PROTOCOL)
}

fn socket_error(error: SocketError) -> Error {
    match error {
        SocketError::Capacity(_) | SocketError::WriteBufferFull(_) => Error::Limit,
        SocketError::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => Error::CoreChannel("peer ended without a closing handshake"),
        SocketError::Io(_) => Error::CoreChannel("transport I/O error"),
        SocketError::Utf8(_) => Error::CoreChannel("invalid UTF-8 frame"),
        _ => Error::CoreChannel("transport or framing error"),
    }
}

fn encode(message: &ClientMessage, limit: usize) -> Result<String> {
    struct BoundedJson {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl std::io::Write for BoundedJson {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("core message byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = BoundedJson {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut output, message).is_err() {
        return Err(if output.exceeded {
            Error::Limit
        } else {
            Error::Request("core command cannot be serialized")
        });
    }
    String::from_utf8(output.bytes).map_err(|_| Error::Request("core command is not UTF-8"))
}
