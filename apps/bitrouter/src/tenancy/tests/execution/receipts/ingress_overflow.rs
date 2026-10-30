//! Actual unknown-length body consumption, not declared-length rejection.
//! Reuse the real receipt fixture and the existing read-only ingress witness.
use super::*;
use axum::body::Bytes;
use bitrouter_ai::tenancy::{
    execution::{Outcome, ReceiptState},
    operation::{DispatchState, Error as TenantError, ErrorCode},
};
use sea_orm::{ConnectionTrait, Statement};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};

const MAX_BODY: usize = 16 * 1024 * 1024;
const SENTINEL_BYTES: usize = 32;
const MAX_RESPONSE: usize = 8 * 1024;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
static PADDING: [u8; 16 * 1024] = [b' '; 16 * 1024];

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn h1_headers(
    harness: &Harness,
    length: Option<usize>,
) -> Result<(OwnedReadHalf, OwnedWriteHalf)> {
    let address = address(harness)?;
    let mut socket = TcpStream::connect(address).await?;
    socket.set_nodelay(true)?;
    // Valid HTTP/1.1 chunked framing; never combine it with Content-Length.
    // https://www.rfc-editor.org/rfc/rfc9112.html#section-7.1
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nConnection: close\r\n",
        harness.path
    );
    match length {
        Some(length) => head.push_str(&format!("Content-Length: {length}\r\n")),
        None => head.push_str("Transfer-Encoding: chunked\r\n"),
    }
    for (name, value) in &harness.headers {
        ensure!(
            !matches!(
                name.as_str(),
                "host" | "connection" | "transfer-encoding" | "content-length"
            ),
            "fixture supplied framing headers"
        );
        head.push_str(&format!("{}: {}\r\n", name.as_str(), value.to_str()?));
    }
    head.push_str("\r\n");
    ensure!(head.len() <= MAX_RESPONSE);
    socket.write_all(head.as_bytes()).await?;
    Ok(socket.into_split())
}

fn address(harness: &Harness) -> Result<SocketAddr> {
    let address: SocketAddr = harness
        .base
        .strip_prefix("http://")
        .context("loopback gateway")?
        .parse()?;
    ensure!(address.ip().is_loopback());
    Ok(address)
}

enum WriteWire {
    H1(OwnedWriteHalf),
    #[cfg(feature = "http2")]
    H2(h2::SendStream<Bytes>),
}
struct Writer {
    wire: WriteWire,
    sent: usize,
}
impl Writer {
    async fn piece(&mut self, bytes: Bytes) -> Result<()> {
        let length = bytes.len();
        ensure!(length > 0 && length <= PADDING.len());
        ensure!(self.sent + length <= MAX_BODY);
        match &mut self.wire {
            WriteWire::H1(socket) => {
                socket
                    .write_all(format!("{length:x}\r\n").as_bytes())
                    .await?;
                socket.write_all(&bytes).await?;
                socket.write_all(b"\r\n").await?;
            }
            #[cfg(feature = "http2")]
            WriteWire::H2(send) => h2_piece(send, bytes, false).await?,
        }
        self.sent += length;
        Ok(())
    }

    async fn prefix(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_vec(request)?;
        ensure!(!json.is_empty() && json.len() <= PADDING.len());
        self.piece(Bytes::from(json)).await?;
        while self.sent < MAX_BODY {
            let length = (MAX_BODY - self.sent).min(PADDING.len());
            self.piece(Bytes::from_static(&PADDING[..length])).await?;
        }
        Ok(())
    }

    async fn finish_overflow(&mut self) -> Result<()> {
        ensure!(self.sent == MAX_BODY);
        match &mut self.wire {
            WriteWire::H1(socket) => {
                socket.write_all(b"1\r\n \r\n0\r\n\r\n").await?;
            }
            #[cfg(feature = "http2")]
            WriteWire::H2(send) => {
                h2_piece(send, Bytes::from_static(b" "), true).await?;
            }
        }
        self.sent += 1;
        Ok(())
    }
}

#[cfg(feature = "http2")]
async fn h2_piece(send: &mut h2::SendStream<Bytes>, mut bytes: Bytes, end: bool) -> Result<()> {
    // Respect flow control for each finite piece; do not queue a16MiB buffer.
    // https://docs.rs/h2/0.4.15/h2/struct.SendStream.html#method.reserve_capacity
    while !bytes.is_empty() {
        send.reserve_capacity(bytes.len());
        let capacity = if send.capacity() == 0 {
            futures::future::poll_fn(|cx| send.poll_capacity(cx))
                .await
                .context("h2 upload closed before payload was sent")??
        } else {
            send.capacity()
        };
        ensure!(capacity > 0);
        let length = capacity.min(bytes.len());
        let last = length == bytes.len();
        send.send_data(bytes.split_to(length), end && last)?;
    }
    Ok(())
}

enum Reader {
    H1(OwnedReadHalf),
    #[cfg(feature = "http2")]
    H2(Option<h2::client::ResponseFuture>),
}
impl Reader {
    async fn response(&mut self) -> Result<Reply> {
        match self {
            Self::H1(reader) => h1_response(reader).await,
            #[cfg(feature = "http2")]
            Self::H2(future) => {
                let response = future.take().context("one actual h2 response")?.await?;
                ensure!(response.version() == ::http::Version::HTTP_2);
                let (parts, mut body) = response.into_parts();
                let mut bytes = Vec::new();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk?;
                    ensure!(chunk.len() <= MAX_RESPONSE.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&chunk);
                    body.flow_control().release_capacity(chunk.len())?;
                }
                Ok(Reply {
                    status: parts.status,
                    headers: parts.headers,
                    body: bytes,
                })
            }
        }
    }
}

async fn h1_response(reader: &mut OwnedReadHalf) -> Result<Reply> {
    let mut received = Vec::new();
    let mut block = [0; 1024];
    let boundary = loop {
        let count = reader.read(&mut block).await?;
        ensure!(count != 0, "response ended before application headers");
        ensure!(received.len() + count <= MAX_RESPONSE);
        received.extend_from_slice(&block[..count]);
        if let Some(offset) = received.windows(4).position(|v| v == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let header = std::str::from_utf8(&received[..boundary])?;
    let mut lines = header.split("\r\n");
    let mut status = lines.next().context("HTTP status")?.split_whitespace();
    ensure!(status.next() == Some("HTTP/1.1"));
    let status = StatusCode::from_u16(status.next().context("status code")?.parse()?)?;
    let mut headers = HeaderMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("HTTP response header")?;
        let name = ::http::header::HeaderName::from_bytes(name.as_bytes())?;
        ensure!(
            !headers.contains_key(&name),
            "duplicate fixture response header"
        );
        headers.insert(name, value.trim().parse()?);
    }
    ensure!(!headers.contains_key("transfer-encoding"));
    let length = headers
        .get("content-length")
        .context("fixed JSON response length")?
        .to_str()?
        .parse::<usize>()?;
    let total = boundary
        .checked_add(length)
        .context("response size overflow")?;
    ensure!(total <= MAX_RESPONSE && received.len() <= total);
    while received.len() < total {
        let length = (total - received.len()).min(block.len());
        let count = reader.read(&mut block[..length]).await?;
        ensure!(count != 0, "application response body truncated");
        received.extend_from_slice(&block[..count]);
    }
    Ok(Reply {
        status,
        headers,
        body: received[boundary..].to_vec(),
    })
}

#[cfg(feature = "http2")]
struct Http2 {
    sender: h2::client::SendRequest<Bytes>,
    driver: AbortOnDropHandle<std::result::Result<(), h2::Error>>,
}
struct Upload {
    writer: Writer,
    reader: Reader,
    #[cfg(feature = "http2")]
    connection: Option<Http2>,
}
impl Upload {
    async fn open(harness: &Harness, http2: bool) -> Result<Self> {
        #[cfg(not(feature = "http2"))]
        ensure!(!http2);
        #[cfg(feature = "http2")]
        if http2 {
            let socket = TcpStream::connect(address(harness)?).await?;
            socket.set_nodelay(true)?;
            // Poll the connection throughout upload and bounded response reads.
            // https://docs.rs/h2/0.4.15/h2/client/index.html
            let (sender, connection) = h2::client::handshake(socket).await?;
            let mut connection = Http2 {
                sender,
                driver: AbortOnDropHandle::new(tokio::spawn(connection)),
            };
            futures::future::poll_fn(|cx| connection.sender.poll_ready(cx)).await?;
            let mut request =
                ::http::Request::post(format!("{}{}", harness.base, harness.path)).body(())?;
            *request.version_mut() = ::http::Version::HTTP_2;
            *request.headers_mut() = harness.headers.clone();
            for name in ["content-length", "transfer-encoding", "connection", "host"] {
                ensure!(!request.headers().contains_key(name));
            }
            request
                .headers_mut()
                .insert("content-type", "application/json".parse()?);
            let (response, send) = connection.sender.send_request(request, false)?;
            return Ok(Self {
                writer: Writer {
                    wire: WriteWire::H2(send),
                    sent: 0,
                },
                reader: Reader::H2(Some(response)),
                connection: Some(connection),
            });
        }
        let (reader, writer) = h1_headers(harness, None).await?;
        Ok(Self {
            writer: Writer {
                wire: WriteWire::H1(writer),
                sent: 0,
            },
            reader: Reader::H1(reader),
            #[cfg(feature = "http2")]
            connection: None,
        })
    }

    async fn overflow(&mut self, request: &Value) -> Result<Reply> {
        let receiving = self.reader.response();
        tokio::pin!(receiving);
        tokio::select! {
            biased;
            response = &mut receiving => {
                let reply = response?;
                anyhow::bail!("application replied {} before sending the full16MiB prefix", reply.status);
            }
            sent = self.writer.prefix(request) => sent?,
        }
        ensure!(self.writer.sent == MAX_BODY);
        // Keep polling this same response future at the exact limit with no
        // EOF/reset. A smaller limit or header-only rejection must not pass.
        if let Ok(response) = tokio::time::timeout(Duration::from_millis(25), &mut receiving).await
        {
            let reply = response?;
            anyhow::bail!(
                "application replied {} before the final body frame",
                reply.status
            );
        }
        self.writer.finish_overflow().await?;
        ensure!(self.writer.sent == MAX_BODY + 1);
        receiving.await
    }

    async fn close(self) -> Result<()> {
        drop((self.writer, self.reader));
        #[cfg(feature = "http2")]
        if let Some(connection) = self.connection {
            drop(connection.sender);
            connection.driver.abort();
            match tokio::time::timeout(TIMEOUT, connection.driver).await? {
                Err(error) if error.is_cancelled() => {}
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error.into()),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

async fn observed(harness: &Harness, count: usize, bytes: usize) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let state = harness
                .fixture
                .service
                .admission
                .ingress_for_test()
                .map_err(|code| anyhow::anyhow!("ingress witness {code:?}"))?;
            ensure!(
                state.2 == 0 && state.3 == 0,
                "body reader admitted execution work"
            );
            if state == (count, bytes, 0, 0) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await??;
    Ok(())
}

async fn campaign(harness: &mut Harness, http2: bool) -> Result<()> {
    observed(harness, 0, 0).await?;
    let (sentinel_reader, mut sentinel_writer) =
        tokio::time::timeout(TIMEOUT, h1_headers(harness, Some(SENTINEL_BYTES))).await??;
    sentinel_writer.write_all(b" ").await?;
    observed(harness, 1, SENTINEL_BYTES).await?;
    let mut upload = tokio::time::timeout(TIMEOUT, Upload::open(harness, http2)).await??;
    observed(harness, 2, MAX_BODY + SENTINEL_BYTES).await?;
    let rejected =
        tokio::time::timeout(TRANSFER_TIMEOUT, upload.overflow(&harness.request)).await??;
    ensure!(rejected.status == StatusCode::PAYLOAD_TOO_LARGE);
    ensure!(!rejected.headers.contains_key("x-bitrouter-request-id"));
    let error: TenantError = serde_json::from_slice(&rejected.body)?;
    ensure!(
        error.code == ErrorCode::PayloadTooLarge
            && error.dispatch_state == DispatchState::NotStarted
    );
    ensure!(error.execution_id.is_none() && error.operation_id.is_none());
    // Both client upload owners are still alive. Only the overflowing request's
    //16MiB and one slot can disappear; a double release would erase the sentinel.
    observed(harness, 1, SENTINEL_BYTES).await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 0);
    ensure!(matches!(
        harness.received.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    ensure!(
        tokio::time::timeout(TIMEOUT, harness.page())
            .await??
            .execution_ids
            .is_empty()
    );
    upload.close().await?;
    observed(harness, 1, SENTINEL_BYTES).await?;
    drop((sentinel_reader, sentinel_writer));
    observed(harness, 0, 0).await?;

    // Exactly16MiB of legal JSON plus whitespace must still execute. Together
    // with the +1 rejection this distinguishes the actual byte limit from an
    // arbitrary smaller limit, invalid JSON or an unknown-length blanket ban.
    let client = Harness::client(http2)?;
    let prefix = serde_json::to_vec(&harness.request)?;
    ensure!(!prefix.is_empty() && prefix.len() < MAX_BODY);
    let mut remaining = MAX_BODY - prefix.len();
    let release = tokio_util::sync::CancellationToken::new();
    let released = release.clone().drop_guard();
    let permit = release.clone();
    // wrap_stream has no exact length, so the request stays unknown-length.
    // https://docs.rs/reqwest/0.13.4/reqwest/struct.Body.html#method.wrap_stream
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from(prefix));
        permit.cancelled().await;
        while remaining > 0 {
            let length = remaining.min(PADDING.len());
            remaining -= length;
            yield Ok(Bytes::from_static(&PADDING[..length]));
        }
    };
    let request = client
        .post(format!("{}{}", harness.base, harness.path))
        .headers(harness.headers.clone())
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(stream))
        .build()?;
    ensure!(!request.headers().contains_key("content-length"));
    let sending = client.execute(request);
    tokio::pin!(sending);
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            biased;
            result = &mut sending => {
                let response = result?;
                anyhow::bail!("positive request replied {} before body release", response.status());
            }
            seen = observed(harness, 1, MAX_BODY) => seen,
        }
    })
    .await??;
    release.cancel();
    let mut response = tokio::time::timeout(TRANSFER_TIMEOUT, &mut sending).await??;
    ensure!(response.status() == StatusCode::OK);
    ensure!(
        response.version()
            == if http2 {
                ::http::Version::HTTP_2
            } else {
                ::http::Version::HTTP_11
            }
    );
    let id = execution_id(&response)?;
    let bytes = tokio::time::timeout(TIMEOUT, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(chunk.len() <= MAX_RESPONSE.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>(bytes)
    })
    .await??;
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value["choices"][0]["message"]["content"] == "receipt reply");
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    observed(harness, 0, 0).await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("actual provider send")?;
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(receipt.execution_id == id && receipt.outcome == Outcome::Completed);
    ensure!(receipt.state == ReceiptState::Committed);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
    ensure!(receipt.usage.input_tokens == Some(11) && receipt.usage.output_tokens == Some(7));
    let page = tokio::time::timeout(TIMEOUT, harness.page()).await??;
    ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
    drop((response, released));
    Ok(())
}

async fn stopped(task: AbortOnDropHandle<std::io::Result<()>>) -> Result<()> {
    task.abort();
    match tokio::time::timeout(TIMEOUT, task).await? {
        Err(error) if error.is_cancelled() => Ok(()),
        Ok(result) => result.map_err(Into::into),
        Err(error) => Err(error.into()),
    }
}

async fn run(http2: bool) -> Result<()> {
    let mut harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let outcome = campaign(&mut harness, http2).await;
    let returned = observed(&harness, 0, 0).await;
    let shutdown = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    // No direct SQL while the actor or uploads can own a transaction. This
    // unfiltered final count also catches accidental receipts outside the page.
    let count = if matches!(&shutdown, Ok(Ok(()))) {
        tokio::time::timeout(TIMEOUT, async {
            let row = harness
                .fixture
                .db
                .query_one(Statement::from_string(
                    harness.fixture.db.get_database_backend(),
                    "SELECT COUNT(*) AS n FROM tenancy_execution_receipts",
                ))
                .await?
                .context("receipt count after actor shutdown")?;
            Ok::<_, anyhow::Error>(Some(row.try_get::<i64>("", "n")?))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    } else {
        Ok(None)
    };
    let gateway = stopped(harness._gateway).await;
    let provider = stopped(harness._provider).await;
    outcome?;
    returned?;
    shutdown??;
    gateway?;
    provider?;
    ensure!(
        count? == Some(1),
        "overflow created receipt effects or positive receipt missing"
    );
    Ok(())
}

#[tokio::test]
async fn chunked_h1_consumed_over16mib_rejects_and_releases_exactly() -> Result<()> {
    run(false).await
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn unknown_length_h2_consumed_over16mib_rejects_and_releases_exactly() -> Result<()> {
    run(true).await
}
