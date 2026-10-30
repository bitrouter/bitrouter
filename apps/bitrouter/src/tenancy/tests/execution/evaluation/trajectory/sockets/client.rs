//! Explicit TCP close / HTTP/2 reset, without a connection pool hiding disconnects.
use super::*;
#[cfg(feature = "http2")]
use axum::body::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Copy)]
pub(super) enum Protocol {
    H1,
    #[cfg(feature = "http2")]
    H2,
}
pub(super) fn protocols() -> Vec<Protocol> {
    #[cfg(feature = "http2")]
    {
        vec![Protocol::H1, Protocol::H2]
    }
    #[cfg(not(feature = "http2"))]
    {
        vec![Protocol::H1]
    }
}
pub(super) enum Client {
    H1(TcpStream),
    #[cfg(feature = "http2")]
    H2(Box<Http2>),
}
#[cfg(feature = "http2")]
pub(super) struct Http2 {
    _sender: h2::client::SendRequest<Bytes>,
    send: h2::SendStream<Bytes>,
    response: Option<h2::client::ResponseFuture>,
    body: Option<h2::RecvStream>,
    driver: AbortOnDropHandle<std::result::Result<(), h2::Error>>,
}
impl Client {
    pub(super) async fn start(
        protocol: Protocol,
        address: std::net::SocketAddr,
        headers: &::http::HeaderMap,
    ) -> Result<Self> {
        let bytes = serde_json::to_vec(&json!({"model":MODEL,"stream":true,
            "messages":[{"role":"user","content":PRIVATE_PROMPT}]}))?;
        let stream = TcpStream::connect(address).await?;
        match protocol {
            Protocol::H1 => {
                let mut stream = stream;
                // https://www.rfc-editor.org/rfc/rfc9112#section-6.2
                let mut head = format!(
                    "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    bytes.len()
                );
                for (name, value) in headers {
                    head.push_str(name.as_str());
                    head.push_str(": ");
                    head.push_str(value.to_str()?);
                    head.push_str("\r\n");
                }
                head.push_str("\r\n");
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&bytes).await?;
                Ok(Self::H1(stream))
            }
            #[cfg(feature = "http2")]
            Protocol::H2 => {
                // https://docs.rs/h2/0.4.15/h2/client/fn.handshake.html
                let (mut sender, connection) = h2::client::handshake(stream).await?;
                let driver = AbortOnDropHandle::new(tokio::spawn(connection));
                futures::future::poll_fn(|cx| sender.poll_ready(cx)).await?;
                let mut request =
                    Request::post(format!("http://{address}/v1/chat/completions")).body(())?;
                *request.headers_mut() = headers.clone();
                request
                    .headers_mut()
                    .insert("content-type", "application/json".parse()?);
                let (response, mut send) = sender.send_request(request, false)?;
                send.send_data(Bytes::from(bytes), true)?;
                Ok(Self::H2(Box::new(Http2 {
                    _sender: sender,
                    send,
                    response: Some(response),
                    body: None,
                    driver,
                })))
            }
        }
    }
    pub(super) async fn prefix(&mut self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, async {
            let mut seen = Vec::new();
            loop {
                let chunk = self
                    .chunk()
                    .await?
                    .context("response ended before first SSE chunk")?;
                seen.extend_from_slice(&chunk);
                ensure!(seen.len() <= 128 * 1024, "response prefix exceeded bound");
                if seen
                    .windows(b"socket-prefix".len())
                    .any(|s| s == b"socket-prefix")
                {
                    return Ok(());
                }
            }
        })
        .await?
    }
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::H1(stream) => {
                let mut bytes = vec![0; 4096];
                let n = stream.read(&mut bytes).await?;
                bytes.truncate(n);
                Ok((n != 0).then_some(bytes))
            }
            #[cfg(feature = "http2")]
            Self::H2(client) => {
                if let Some(response) = client.response.take() {
                    client.body = Some(response.await?.into_body());
                }
                let body = client.body.as_mut().context("missing h2 body")?;
                match body.data().await {
                    Some(chunk) => {
                        let bytes = chunk?;
                        body.flow_control().release_capacity(bytes.len())?;
                        Ok(Some(bytes.to_vec()))
                    }
                    None => Ok(None),
                }
            }
        }
    }
    pub(super) async fn rejection(&mut self) -> Result<bitrouter_ai::tenancy::operation::Error> {
        tokio::time::timeout(TIMEOUT, async {
            let mut bytes = Vec::new();
            while let Some(chunk) = self.chunk().await? {
                bytes.extend_from_slice(&chunk);
                ensure!(bytes.len() <= 128 * 1024, "rejection exceeded bound");
            }
            let body = match self {
                Self::H1(_) => {
                    let start = bytes
                        .windows(4)
                        .position(|s| s == b"\r\n\r\n")
                        .context("HTTP response headers")?
                        + 4;
                    ensure!(
                        bytes.starts_with(b"HTTP/1.1 401"),
                        "unexpected rejection status"
                    );
                    &bytes[start..]
                }
                #[cfg(feature = "http2")]
                Self::H2(_) => bytes.as_slice(),
            };
            Ok(serde_json::from_slice(body)?)
        })
        .await?
    }
    pub(super) async fn disconnect(self) -> Result<()> {
        match self {
            Self::H1(mut stream) => {
                stream.shutdown().await?;
                drop(stream);
            }
            #[cfg(feature = "http2")]
            Self::H2(mut client) => {
                // https://docs.rs/h2/0.4.15/h2/struct.SendStream.html#method.send_reset
                client.send.send_reset(h2::Reason::CANCEL);
                client.driver.abort();
                match client.driver.await {
                    Err(error) if error.is_cancelled() => {}
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error.into()),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}
