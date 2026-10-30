//! Section C only: real slow uploads, unchanged application limits/deadline.
//! Declared/reserved bytes are not allocations or an RSS bound. No fault writer.
use super::*;
#[cfg(feature = "http2")]
use axum::body::Bytes;
use bitrouter_ai::tenancy::operation::{DispatchState, Error as TenantError, ErrorCode};
use sea_orm::{ConnectionTrait, Statement};
use std::{net::SocketAddr, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};

const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_RESERVED: usize = 64 * 1024 * 1024;
const MAX_UPLOADS: usize = 128;
const MAX_RESPONSE: usize = 8 * 1024;
const CHILD_MODE: &str = "BITROUTER_RECEIPT_INGRESS_DEADLINE_CHILD";
const CHILD_NAME: &str =
    "tenancy::tests::execution::receipts::ingress_socket::absolute_deadline_child";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    H1,
    #[cfg(feature = "http2")]
    H2,
}
fn wires() -> Vec<Wire> {
    #[cfg(feature = "http2")]
    {
        vec![Wire::H1, Wire::H2]
    }
    #[cfg(not(feature = "http2"))]
    {
        vec![Wire::H1]
    }
}
impl Wire {
    fn http2(self) -> bool {
        match self {
            Self::H1 => false,
            #[cfg(feature = "http2")]
            Self::H2 => true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Framing {
    Length(usize),
    Unknown,
}
impl Framing {
    fn bound(self) -> usize {
        match self {
            Self::Length(bytes) => bytes,
            Self::Unknown => MAX_BODY,
        }
    }
}

struct Clients {
    address: SocketAddr,
    #[cfg(feature = "http2")]
    multiplexed: Option<Http2>,
}
#[cfg(feature = "http2")]
struct Http2 {
    sender: h2::client::SendRequest<Bytes>,
    driver: AbortOnDropHandle<std::result::Result<(), h2::Error>>,
}
impl Clients {
    async fn new(harness: &Harness, wire: Wire) -> Result<Self> {
        let address: SocketAddr = harness
            .base
            .strip_prefix("http://")
            .context("loopback gateway URL")?
            .parse()?;
        ensure!(address.ip().is_loopback());
        #[cfg(not(feature = "http2"))]
        ensure!(wire == Wire::H1);
        #[cfg(feature = "http2")]
        let multiplexed = if wire == Wire::H2 {
            let socket = tokio::time::timeout(TIMEOUT, TcpStream::connect(address)).await??;
            socket.set_nodelay(true)?;
            // Zero until peer SETTINGS arrive; never mistake the client's
            // initial unlimited placeholder for the server's advertised limit.
            // https://docs.rs/h2/latest/h2/client/index.html
            let (mut sender, connection) = tokio::time::timeout(
                TIMEOUT,
                h2::client::Builder::new()
                    .initial_max_send_streams(0)
                    // Client-only tombstone capacity for three bounded cycles;
                    // this does not change the server/application stream limit.
                    .max_concurrent_reset_streams(512)
                    .handshake(socket),
            )
            .await??;
            let driver = AbortOnDropHandle::new(tokio::spawn(connection));
            tokio::time::timeout(TIMEOUT, async {
                // poll_ready alone is initially Ready even with a zero limit;
                // observe the actual peer SETTINGS update, not just readiness.
                while sender.current_max_send_streams() == 0 {
                    futures::future::poll_fn(|cx| sender.poll_ready(cx)).await?;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            ensure!(
                sender.current_max_send_streams() > MAX_UPLOADS,
                "h2 transport allows fewer than129 streams; this cannot qualify the application128 bound"
            );
            eprintln!(
                "ingress h2 peer max_send_streams={}",
                sender.current_max_send_streams()
            );
            Some(Http2 { sender, driver })
        } else {
            None
        };
        Ok(Self {
            address,
            #[cfg(feature = "http2")]
            multiplexed,
        })
    }
    async fn open(
        &mut self,
        harness: &Harness,
        framing: Framing,
        first_byte: bool,
    ) -> Result<Upload> {
        let mut upload = tokio::time::timeout(TIMEOUT, self.open_headers(harness, framing))
            .await
            .context("ingress request headers deadline")?
            .with_context(|| format!("ingress request headers framing={framing:?}"))?;
        if first_byte
            && let Err(error) = tokio::time::timeout(TIMEOUT, upload.writer.one())
                .await
                .context("ingress first DATA deadline")
                .and_then(|result| result)
        {
            // An intended held upload must still fail if the application
            // rejects before its first byte; show the real reply, not a reset.
            let response = tokio::time::timeout(TIMEOUT, upload.reader.response())
                .await
                .context("early response after first DATA failure")
                .and_then(|result| result);
            let detail = match response {
                Ok(response) => format!(
                    "typed status={} code={:?}",
                    response.status, response.error.code
                ),
                Err(response) => format!("response unavailable: {response:#}"),
            };
            return Err(error.context(format!("ingress first DATA framing={framing:?}; {detail}")));
        }
        Ok(upload)
    }
    async fn open_headers(&mut self, harness: &Harness, framing: Framing) -> Result<Upload> {
        for name in ["host", "connection", "transfer-encoding", "content-length"] {
            ensure!(
                !harness.headers.contains_key(name),
                "fixture supplied framing header"
            );
        }
        #[cfg(feature = "http2")]
        if let Some(connection) = self.multiplexed.as_mut() {
            futures::future::poll_fn(|cx| connection.sender.poll_ready(cx)).await?;
            let mut request =
                ::http::Request::post(format!("http://{}{}", self.address, harness.path))
                    .body(())?;
            *request.version_mut() = ::http::Version::HTTP_2;
            *request.headers_mut() = harness.headers.clone();
            request
                .headers_mut()
                .insert("content-type", "application/json".parse()?);
            if let Framing::Length(length) = framing {
                request
                    .headers_mut()
                    .insert("content-length", length.to_string().parse()?);
            }
            // h2 encodes method/scheme/authority/path as pseudo headers. No
            // HTTP/1 connection or transfer-encoding header enters this request.
            let (response, send) = connection.sender.send_request(request, false)?;
            return Ok(Upload {
                reader: Reader::H2(Some(response)),
                writer: Writer {
                    wire: WriteWire::H2(send),
                    framing,
                    sent: 0,
                },
            });
        }
        let mut socket = TcpStream::connect(self.address).await?;
        socket.set_nodelay(true)?;
        // HTTP/1.1 request framing follows the wire grammar in RFC 9112.
        // https://www.rfc-editor.org/rfc/rfc9112.html#section-2.1
        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
            harness.path, self.address
        );
        match framing {
            Framing::Length(length) => head.push_str(&format!("Content-Length: {length}\r\n")),
            Framing::Unknown => head.push_str("Transfer-Encoding: chunked\r\n"),
        }
        for (name, value) in &harness.headers {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), value.to_str()?));
        }
        head.push_str("\r\n");
        ensure!(head.len() <= MAX_RESPONSE);
        socket.write_all(head.as_bytes()).await?;
        let (reader, writer) = socket.into_split();
        Ok(Upload {
            reader: Reader::H1(reader),
            writer: Writer {
                wire: WriteWire::H1(writer),
                framing,
                sent: 0,
            },
        })
    }
    async fn close(self) -> Result<()> {
        #[cfg(feature = "http2")]
        if let Some(connection) = self.multiplexed {
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
struct Upload {
    reader: Reader,
    writer: Writer,
}
impl Upload {
    fn disconnect(mut self) {
        self.writer.cancel();
    }
}
enum Reader {
    H1(OwnedReadHalf),
    #[cfg(feature = "http2")]
    H2(Option<h2::client::ResponseFuture>),
}
enum WriteWire {
    H1(OwnedWriteHalf),
    #[cfg(feature = "http2")]
    H2(h2::SendStream<Bytes>),
}
struct Writer {
    wire: WriteWire,
    framing: Framing,
    sent: usize,
}
impl Writer {
    async fn one(&mut self) -> Result<()> {
        // At most128 actual bytes per upload; no declared MiB allocation.
        ensure!(self.sent < 128 && self.sent < self.framing.bound());
        match &mut self.wire {
            WriteWire::H1(socket) => match self.framing {
                Framing::Length(_) => socket.write_all(b" ").await?,
                // https://www.rfc-editor.org/rfc/rfc9112.html#section-7.1
                Framing::Unknown => socket.write_all(b"1\r\n \r\n").await?,
            },
            #[cfg(feature = "http2")]
            WriteWire::H2(send) => {
                send.reserve_capacity(1);
                let available = futures::future::poll_fn(|cx| send.poll_capacity(cx))
                    .await
                    .context("h2 upload closed")??;
                ensure!(available >= 1);
                send.send_data(Bytes::from_static(b" "), false)?;
            }
        }
        self.sent += 1;
        Ok(())
    }
    fn cancel(&mut self) {
        #[cfg(feature = "http2")]
        if let WriteWire::H2(send) = &mut self.wire {
            send.send_reset(h2::Reason::CANCEL);
        }
        // H1 closes both halves when Upload drops. A premature fixed body or
        // unfinished chunked body is a disconnect, never a legal completed JSON.
    }
}
struct Rejection {
    status: StatusCode,
    error: TenantError,
}
impl Reader {
    async fn response(&mut self) -> Result<Rejection> {
        match self {
            Self::H1(reader) => h1_response(reader).await,
            #[cfg(feature = "http2")]
            Self::H2(future) => {
                let response = future
                    .take()
                    .context("one h2 response")?
                    .await
                    .context("h2 response headers")?;
                ensure!(response.version() == ::http::Version::HTTP_2);
                let status = response.status();
                // Axum Json/Hyper provide an exact Content-Length here. It is
                // independent evidence of completion when h2 reports the reset
                // used to stop an unfinished request after its early response.
                let mut lengths = response
                    .headers()
                    .get_all(::http::header::CONTENT_LENGTH)
                    .iter();
                let length = lengths
                    .next()
                    .context("h2 application error missing Content-Length")?
                    .to_str()?;
                ensure!(length.bytes().all(|byte| byte.is_ascii_digit()) && !length.is_empty());
                let length: usize = length.parse().context("h2 response Content-Length")?;
                ensure!(
                    lengths.next().is_none() && (1..=MAX_RESPONSE).contains(&length),
                    "ambiguous or unbounded h2 response length"
                );
                let mut body = response.into_body();
                let mut bytes = Vec::with_capacity(length);
                while let Some(chunk) = body.data().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        // h2 0.4.15 maybe_cancel emits NO_ERROR after an early
                        // response; recv_reset can replace HalfClosedRemote even
                        // after its END_STREAM. Only a complete framed response
                        // on THIS stream can survive this specific remote reset.
                        Err(error)
                            if error.is_remote()
                                && error.is_reset()
                                && error.reason() == Some(h2::Reason::NO_ERROR)
                                && bytes.len() == length =>
                        {
                            break;
                        }
                        Err(error) => {
                            return Err(anyhow::Error::new(error).context(format!(
                                "h2 response DATA bytes={}/{length}",
                                bytes.len()
                            )));
                        }
                    };
                    ensure!(
                        chunk.len() <= length.saturating_sub(bytes.len()),
                        "h2 response exceeds Content-Length"
                    );
                    bytes.extend_from_slice(&chunk);
                    body.flow_control()
                        .release_capacity(chunk.len())
                        .context("h2 response flow release")?;
                }
                ensure!(
                    bytes.len() == length,
                    "h2 application error body truncated: {}/{length}",
                    bytes.len()
                );
                Ok(Rejection {
                    status,
                    error: serde_json::from_slice(&bytes)
                        .context("h2 complete body is not a tenancy error")?,
                })
            }
        }
    }
}
async fn h1_response(reader: &mut OwnedReadHalf) -> Result<Rejection> {
    // https://www.rfc-editor.org/rfc/rfc9112.html#section-6.2
    let mut received = Vec::new();
    let mut block = [0; 1024];
    let boundary = loop {
        let n = reader.read(&mut block).await?;
        ensure!(n > 0, "H1 response ended before headers");
        ensure!(received.len() + n <= MAX_RESPONSE);
        received.extend_from_slice(&block[..n]);
        if let Some(offset) = received.windows(4).position(|v| v == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let header = std::str::from_utf8(&received[..boundary])?;
    let mut lines = header.split("\r\n");
    let mut status = lines.next().context("status line")?.split_whitespace();
    ensure!(status.next() == Some("HTTP/1.1"));
    let status = StatusCode::from_u16(status.next().context("status")?.parse()?)?;
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("response header")?;
        ensure!(
            !name.eq_ignore_ascii_case("transfer-encoding"),
            "fixture expects fixed Json failure body"
        );
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none());
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    // Failure::into_response uses axum Json with a known exact length. A peer
    // framing error / connection reset is not an application-limit rejection.
    let length = length.context("fixed application error length")?;
    let total = boundary
        .checked_add(length)
        .context("response length overflow")?;
    ensure!(total <= MAX_RESPONSE && received.len() <= total);
    while received.len() < total {
        let remaining = (total - received.len()).min(block.len());
        let n = reader.read(&mut block[..remaining]).await?;
        ensure!(n > 0, "H1 response body truncated");
        received.extend_from_slice(&block[..n]);
    }
    Ok(Rejection {
        status,
        error: serde_json::from_slice(&received[boundary..])?,
    })
}
fn rejected(response: Rejection, code: ErrorCode) -> Result<()> {
    let expected = if code == ErrorCode::PayloadTooLarge {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::TOO_MANY_REQUESTS
    };
    ensure!(response.status == expected && response.error.code == code);
    ensure!(
        response.error.dispatch_state == DispatchState::NotStarted
            && response.error.execution_id.is_none()
    );
    Ok(())
}

fn snapshot(harness: &Harness) -> Result<(usize, usize, usize, usize)> {
    harness
        .fixture
        .service
        .admission
        .ingress_for_test()
        .map_err(|code| anyhow::anyhow!("ingress witness {code:?}"))
}
async fn observed(harness: &Harness, count: usize, bytes: usize) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let actual = snapshot(harness)?;
            ensure!(
                actual.2 == 0 && actual.3 == 0,
                "unfinished upload admitted execution work"
            );
            if actual == (count, bytes, 0, 0) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await??;
    Ok(())
}
async fn refused(
    clients: &mut Clients,
    harness: &Harness,
    framing: Framing,
    code: ErrorCode,
    expected: (usize, usize),
) -> Result<()> {
    // No body byte and no client cancellation before the application response.
    let mut upload = clients.open(harness, framing, false).await?;
    let response = tokio::time::timeout(TIMEOUT, upload.reader.response())
        .await
        .context("application refusal response deadline")?
        .with_context(|| {
            format!("application refusal framing={framing:?} retained={expected:?}")
        })?;
    rejected(response, code)?;
    observed(harness, expected.0, expected.1).await?;
    upload.disconnect();
    observed(harness, expected.0, expected.1).await
}
async fn receipt_count(harness: &Harness) -> Result<i64> {
    tokio::time::timeout(TIMEOUT, async {
        let row = harness
            .fixture
            .db
            .query_one(Statement::from_string(
                harness.fixture.db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM tenancy_execution_receipts",
            ))
            .await?
            .context("receipt count")?;
        Ok(row.try_get("", "n")?)
    })
    .await?
}
async fn no_execution(harness: &Harness) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        ensure!(receipt_count(harness).await? == 0);
        ensure!(harness.page().await?.execution_ids.is_empty());
        let (_, _, active, queued) = snapshot(harness)?;
        ensure!(active == 0 && queued == 0);
        Ok(())
    })
    .await?
}

async fn harness() -> Result<Harness> {
    Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await
}
async fn setup(wire: Wire) -> Result<(Harness, Clients)> {
    let harness = harness().await?;
    match Clients::new(&harness, wire).await {
        Ok(clients) => Ok((harness, clients)),
        Err(error) => {
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
            Err(error)
        }
    }
}
async fn healthy_request(harness: &Harness, wire: Wire) -> Result<()> {
    let client = Harness::client(wire.http2())?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    ensure!(
        response.version()
            == if wire.http2() {
                ::http::Version::HTTP_2
            } else {
                ::http::Version::HTTP_11
            }
    );
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    observed(harness, 0, 0).await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(
        receipt.attempts.len() == 1
            && receipt.outcome == bitrouter_ai::tenancy::execution::Outcome::Completed
    );
    Ok(())
}
async fn finish(harness: Harness, clients: Clients, outcome: Result<()>) -> Result<()> {
    let closed = clients.close().await;
    let returned = observed(&harness, 0, 0).await;
    let shutdown = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    outcome?;
    closed?;
    returned?;
    shutdown??;
    Ok(())
}

#[tokio::test]
async fn h1_and_h2_upload_count128_reject129_and_recycle_without_executions() -> Result<()> {
    for wire in wires() {
        let (harness, mut clients) = setup(wire).await?;
        let outcome: Result<()> = async {
            for _ in 0..3 {
                let mut held = Vec::with_capacity(MAX_UPLOADS);
                for count in 1..=MAX_UPLOADS {
                    held.push(clients.open(&harness, Framing::Length(2), true).await?);
                    observed(&harness, count, count * 2).await?;
                }
                refused(
                    &mut clients,
                    &harness,
                    Framing::Length(2),
                    ErrorCode::CapacityExceeded,
                    (MAX_UPLOADS, MAX_UPLOADS * 2),
                )
                .await?;
                observed(&harness, MAX_UPLOADS, MAX_UPLOADS * 2).await?;
                held.pop().context("one held upload")?.disconnect();
                observed(&harness, MAX_UPLOADS - 1, (MAX_UPLOADS - 1) * 2).await?;
                held.push(clients.open(&harness, Framing::Length(2), true).await?);
                observed(&harness, MAX_UPLOADS, MAX_UPLOADS * 2).await?;
                while let Some(upload) = held.pop() {
                    upload.disconnect();
                }
                observed(&harness, 0, 0).await?;
                no_execution(&harness).await?;
            }
            healthy_request(&harness, wire).await
        }
        .await;
        finish(harness, clients, outcome).await?;
    }
    Ok(())
}

#[tokio::test]
async fn h1_and_h2_upload_reservations64mib_and_declared16mib_boundary() -> Result<()> {
    for wire in wires() {
        let (harness, mut clients) = setup(wire).await?;
        let outcome: Result<()> = async {
            for framing in [Framing::Unknown, Framing::Length(MAX_BODY)] {
                for _ in 0..3 {
                    let mut held = Vec::with_capacity(4);
                    for count in 1..=4 {
                        held.push(clients.open(&harness, framing, true).await?);
                        observed(&harness, count, count * MAX_BODY).await?;
                    }
                    ensure!(snapshot(&harness)?.1 == MAX_RESERVED);
                    refused(
                        &mut clients,
                        &harness,
                        framing,
                        ErrorCode::CapacityExceeded,
                        (4, MAX_RESERVED),
                    )
                    .await?;
                    // One declared byte over the exact aggregate budget, with
                    // no body byte sent: application refusal precedes parsing.
                    refused(
                        &mut clients,
                        &harness,
                        Framing::Length(1),
                        ErrorCode::CapacityExceeded,
                        (4, MAX_RESERVED),
                    )
                    .await?;
                    observed(&harness, 4, MAX_RESERVED).await?;
                    held.pop().context("one reserved upload")?.disconnect();
                    observed(&harness, 3, 3 * MAX_BODY).await?;
                    held.push(clients.open(&harness, framing, true).await?);
                    observed(&harness, 4, MAX_RESERVED).await?;
                    while let Some(upload) = held.pop() {
                        upload.disconnect();
                    }
                    observed(&harness, 0, 0).await?;
                    no_execution(&harness).await?;
                }
            }
            refused(
                &mut clients,
                &harness,
                Framing::Length(MAX_BODY + 1),
                ErrorCode::PayloadTooLarge,
                (0, 0),
            )
            .await?;
            observed(&harness, 0, 0).await?;
            no_execution(&harness).await?;
            healthy_request(&harness, wire).await
        }
        .await;
        finish(harness, clients, outcome).await?;
    }
    Ok(())
}

#[tokio::test]
async fn closing_service_during_real_uploads_does_not_forge_reservation_release() -> Result<()> {
    for wire in wires() {
        let (harness, mut clients) = setup(wire).await?;
        let outcome: Result<()> = async {
            let mut held = Vec::with_capacity(3);
            for count in 1..=3 {
                held.push(clients.open(&harness, Framing::Unknown, true).await?);
                observed(&harness, count, count * MAX_BODY).await?;
            }
            no_execution(&harness).await?;
            // wait_drained currently counts execution leases/queue, not ingress
            // bodies. Do not assert that a shutdown ACK alone cancelled uploads.
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
            let snapshot = snapshot(&harness)?;
            ensure!(snapshot.2 == 0 && snapshot.3 == 0);
            let mut rejected = clients.open(&harness, Framing::Length(2), false).await?;
            let response = tokio::time::timeout(TIMEOUT, rejected.reader.response()).await??;
            ensure!(
                !response.status.is_success()
                    && response.error.dispatch_state == DispatchState::NotStarted
                    && response.error.execution_id.is_none()
            );
            rejected.disconnect();
            while let Some(upload) = held.pop() {
                upload.disconnect();
            }
            observed(&harness, 0, 0).await?;
            ensure!(harness.calls.load(Ordering::SeqCst) == 0);
            ensure!(receipt_count(&harness).await? == 0);
            Ok(())
        }
        .await;
        finish(harness, clients, outcome).await?;
    }
    Ok(())
}

async fn drip(wire: Wire) -> Result<()> {
    let (harness, mut clients) = setup(wire).await?;
    let outcome: Result<()> = async {
        let started = std::time::Instant::now();
        let Upload { mut reader, mut writer } = clients.open(&harness, Framing::Length(128), true).await?;
        observed(&harness, 1, 128).await?;
        let mut response = Box::pin(reader.response());
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        let mut last_progress = started.elapsed();
        let (response, sent, last_progress) = tokio::time::timeout(Duration::from_secs(70), async {
            loop {
                tokio::select! {
                    biased;
                    result = &mut response => break Ok::<_,anyhow::Error>((result?, writer.sent, last_progress)),
                    _ = ticker.tick() => {
                        if started.elapsed() < Duration::from_secs(55) {
                            ensure!(snapshot(&harness)? == (1, 128, 0, 0), "upload ended before its absolute deadline");
                        }
                        match tokio::time::timeout(Duration::from_secs(2), writer.one()).await {
                            Ok(Ok(())) => last_progress = started.elapsed(),
                            _ => {
                                // Socket closure alone cannot pass: still require
                                // the actual typed application deadline response.
                                let result = tokio::time::timeout(Duration::from_secs(2), &mut response).await??;
                                break Ok((result, writer.sent, last_progress));
                            },
                        }
                    },
                }
            }
        }).await??;
        let elapsed = started.elapsed();
        rejected(response, ErrorCode::CapacityExceeded)?;
        ensure!(elapsed >= Duration::from_secs(60) && elapsed <= Duration::from_secs(70), "deadline elapsed={elapsed:?}");
        ensure!((55..128).contains(&sent) && elapsed.saturating_sub(last_progress) <= Duration::from_secs(3),
            "not a continuing drip: bytes={sent}, last={last_progress:?}, elapsed={elapsed:?}");
        // Keep the writer alive until counters return. Client cancellation
        // must not receive credit for the server's deadline cleanup.
        observed(&harness, 0, 0).await?;
        no_execution(&harness).await?;
        writer.cancel();
        drop(writer);
        eprintln!("ingress deadline {wire:?}: elapsed={elapsed:?}, sent={sent}, last_progress={last_progress:?}");
        healthy_request(&harness, wire).await
    }.await;
    finish(harness, clients, outcome).await
}

async fn bounded_output(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(16 * 1024 + 1).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "deadline child output exceeded bound"
    );
    Ok(bytes)
}
async fn deadline_child(mode: &str) -> Result<()> {
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD_NAME,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MODE, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;
    let output = AbortOnDropHandle::new(tokio::spawn(bounded_output(stdout)));
    let errors = AbortOnDropHandle::new(tokio::spawn(bounded_output(stderr)));
    let status = match tokio::time::timeout(Duration::from_secs(82), child.wait()).await {
        Ok(status) => status?,
        Err(error) => {
            let killed = child.start_kill();
            let reaped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            killed?;
            reaped??;
            return Err(error.into());
        }
    };
    let output = tokio::time::timeout(Duration::from_secs(2), output).await???;
    let errors = tokio::time::timeout(Duration::from_secs(2), errors).await???;
    ensure!(
        status.success(),
        "deadline child failed: {} {}",
        String::from_utf8_lossy(&output),
        String::from_utf8_lossy(&errors)
    );
    ensure!(
        String::from_utf8_lossy(&output).contains("1 passed")
            && String::from_utf8_lossy(&errors).contains("ingress deadline "),
        "child did not execute deadline evidence"
    );
    Ok(())
}
#[tokio::test]
async fn h1_real_drip_keeps_the_original60s_deadline() -> Result<()> {
    deadline_child("h1").await
}
#[cfg(feature = "http2")]
#[tokio::test]
async fn h2_real_drip_keeps_the_original60s_deadline() -> Result<()> {
    deadline_child("h2").await
}

#[tokio::test]
#[ignore = "invoked by bounded parent with a per-child environment; no global env mutation"]
async fn absolute_deadline_child() -> Result<()> {
    let wire = match std::env::var(CHILD_MODE)?.as_str() {
        "h1" => Wire::H1,
        #[cfg(feature = "http2")]
        "h2" => Wire::H2,
        _ => anyhow::bail!("unsupported deadline child protocol"),
    };
    drip(wire).await
}

#[cfg(feature = "http2")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EarlyReply {
    Complete,
    Truncated,
    InvalidJson,
    MissingLength,
    ResetOnly,
}

#[cfg(feature = "http2")]
async fn early_reply(case: EarlyReply) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = tokio_util::sync::CancellationToken::new();
    let stopped = stop.clone();
    let peer = AbortOnDropHandle::new(tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut connection = h2::server::handshake(socket).await?;
        let (request, mut response) =
            connection.accept().await.context("one fixture request")??;
        ensure!(
            !request.body().is_end_stream(),
            "fixture upload must remain unfinished"
        );
        if case == EarlyReply::ResetOnly {
            response.send_reset(h2::Reason::NO_ERROR);
        } else {
            let bytes = if case == EarlyReply::InvalidJson {
                b"{}".to_vec()
            } else {
                serde_json::to_vec(&TenantError {
                    code: ErrorCode::CapacityExceeded,
                    message: "fixture early response".into(),
                    dispatch_state: DispatchState::NotStarted,
                    operation_id: None,
                    execution_id: None,
                })?
            };
            let mut headers = ::http::Response::builder().status(StatusCode::TOO_MANY_REQUESTS);
            if case != EarlyReply::MissingLength {
                headers = headers.header(
                    ::http::header::CONTENT_LENGTH,
                    bytes.len() + usize::from(case == EarlyReply::Truncated),
                );
            }
            let mut body = response.send_response(headers.body(())?, false)?;
            body.send_data(Bytes::from(bytes), true)?;
        }
        // The real h2 server schedules NO_ERROR AFTER queued response frames
        // when its send half is closed and unread request input is dropped.
        drop((request, response));
        tokio::select! {
            biased;
            () = stopped.cancelled() => Ok::<_,anyhow::Error>(()),
            incoming = connection.accept() => {
                ensure!(incoming.is_none(), "unexpected additional fixture stream");
                Ok(())
            },
        }
    }));
    let socket = TcpStream::connect(address).await?;
    let (mut sender, connection) = h2::client::handshake(socket).await?;
    let driver = AbortOnDropHandle::new(tokio::spawn(connection));
    let result: Result<()> = async {
        futures::future::poll_fn(|cx| sender.poll_ready(cx)).await?;
        let request = ::http::Request::post(format!("http://{address}/early"))
            .header(::http::header::CONTENT_LENGTH, "2")
            .body(())?;
        let (response, mut upload) = sender.send_request(request, false)?;
        // Force the reset to have actually arrived before reading buffered
        // headers/DATA. Thus Complete exercises the original failing order.
        let reset = tokio::time::timeout(
            TIMEOUT,
            futures::future::poll_fn(|cx| upload.poll_reset(cx)),
        )
        .await??;
        if case == EarlyReply::Truncated {
            // Depending on which terminal frame is driven first, the client
            // records its own length error or the peer's following NO_ERROR.
            // Neither can qualify the truncated response below.
            ensure!(matches!(
                reset,
                h2::Reason::PROTOCOL_ERROR | h2::Reason::NO_ERROR
            ));
        } else {
            ensure!(
                reset == h2::Reason::NO_ERROR,
                "unexpected reset for {case:?}: {reset:?}"
            );
        }
        let actual = tokio::time::timeout(TIMEOUT, Reader::H2(Some(response)).response()).await?;
        if case == EarlyReply::Complete {
            rejected(actual?, ErrorCode::CapacityExceeded)?;
        } else {
            ensure!(
                actual.is_err(),
                "incomplete/untyped response accepted for {case:?}"
            );
        }
        // Reset alone never supplies the application status/body. In particular
        // truncated valid JSON cannot satisfy its larger advertised byte count.
        drop(upload);
        Ok(())
    }
    .await;
    stop.cancel();
    drop(sender);
    driver.abort();
    let driver_done = tokio::time::timeout(TIMEOUT, driver).await;
    let peer_done = tokio::time::timeout(TIMEOUT, peer).await;
    result?;
    match driver_done? {
        Err(error) if error.is_cancelled() => {}
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(error) => return Err(error.into()),
    }
    peer_done???;
    Ok(())
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn h2_early_reply_requires_complete_bounded_typed_body_after_actual_reset() -> Result<()> {
    for case in [
        EarlyReply::Complete,
        EarlyReply::Truncated,
        EarlyReply::InvalidJson,
        EarlyReply::MissingLength,
        EarlyReply::ResetOnly,
    ] {
        tokio::time::timeout(TIMEOUT, early_reply(case))
            .await
            .with_context(|| format!("early reply fixture deadline {case:?}"))?
            .with_context(|| format!("early reply fixture {case:?}"))?;
    }
    Ok(())
}
