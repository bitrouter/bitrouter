//! One stock-process A07 candidate. Observe real socket backpressure, resume
//! the reader to demonstrate progress, then leave it open for the write stall.

use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::watch;

use super::super as recovery;
use super::*;

const STALL_MS: u64 = 10_000;
const CHUNK_TEXT: usize = 16 * 1024;
const MAX_FRAMES: usize = 2048;
const MAX_WIRE: usize = 40 * 1024 * 1024;
const MAX_RESUME: usize = 16 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_stalled_response_reader_releases_local_capacity_without_provider_success()
-> Result<()> {
    check_wire_decoder()?;
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let probe = Arc::new(Probe::new());
    let mut expected = ExitExpectations::new();
    let mut clients = recovery::Clients::default();
    let mut provider = match start_provider(probe.clone()).await {
        Ok(provider) => provider,
        Err(error) => {
            return finish_empty_fixture_with_database(database, fixture, Err(error), &expected)
                .await;
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(85), async {
        let world = start_world(&mut fixture, &provider.base, &mut expected).await?;
        world.configure_limits().await?;
        exercise(&world, &mut provider, &mut clients, &probe).await?;
        clients.consumed()?;
        fixture.no_local_files()?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("stalled-reader process campaign deadline exceeded")
    .and_then(|result| result);
    let count = result.as_ref().ok().map(|_| 4);
    // Client, child and provider owners outlive the timed body. On any error,
    // raw peers drop, all client tasks join, then owned gateways stop. Provider
    // operations/HTTP roots remain alive until those child joins complete.
    let result = combine_results(result, vec![clients.finish().await]);
    let result = finish_children(
        &mut fixture.children,
        result,
        &expected,
        &failure_log_prefix(),
    )
    .await;
    let result = combine_results(result, vec![provider.finish(count).await]);
    finish_empty_fixture_with_database(database, fixture, result, &expected).await
}

async fn exercise(
    w: &World,
    provider: &mut Provider,
    clients: &mut recovery::Clients,
    probe: &Arc<Probe>,
) -> Result<()> {
    let socket = TcpSocket::new_v4()?;
    socket.set_recv_buffer_size(4096)?;
    ensure!(
        socket.recv_buffer_size()? <= 64 * 1024,
        "receive buffer setting was not bounded"
    );
    let mut reader = socket.connect(w.replicas[0]).await?;
    let body = serde_json::to_vec(
        &json!({"model":"account-a:mock","messages":[{"role":"user","content":"held"}],"stream":true}),
    )?;
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nIdempotency-Key: held\r\nx-bitrouter-request-id: held\r\nConnection: close\r\n\r\n",
        w.replicas[0],
        w.keys[0].secret,
        body.len()
    );
    reader.write_all(head.as_bytes()).await?;
    reader.write_all(&body).await?;
    let arrival = provider.arrival("held", "account", true).await?;
    let id = arrival.execution.clone();
    let head = tokio::time::timeout(Duration::from_secs(4), read_head(&mut reader)).await??;
    head.expect_status(StatusCode::OK)?;
    head.identity(&id, "held")?;
    ensure!(
        head.header("content-type")?
            .starts_with("text/event-stream")
    );
    ensure!(
        head.header("transfer-encoding")?
            .eq_ignore_ascii_case("chunked")
    );
    ensure!(!head.headers.contains_key("content-length"));
    let mut wire = StreamWire::default();
    tokio::time::timeout(Duration::from_secs(4), async {
        while !wire.held {
            ensure!(
                wire.read(&mut reader).await?,
                "stream closed before initial delta"
            );
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    let running = recovery::view(w, &id).await?;
    ensure!(matches!(running.state, ExecutionState::Running));
    ensure!(
        running.attempts.len() == 1
            && matches!(running.attempts[0].state, AttemptState::Dispatched)
    );

    probe.start.cancel();
    // Neither an unread tiny response nor elapsed silence is the barrier:
    // this is Poll::Pending on the actual first provider TCP connection.
    let first = probe.stalled(1024 * 1024, 1).await?;
    reject_locally(w, "while-stalled").await?;
    recovery::progress(w, provider, clients, 3, "other:mock", "other", "survivor").await?;
    let held = probe.snapshot();
    ensure!(
        held.pending && held.written == first.written && !held.body_dropped,
        "provider did not stay backpressured across survivor completion: first={first:?}, held={held:?}"
    );
    ensure!(provider.calls() == 2);

    // A bounded reader resume must restore writes on that SAME upstream TCP
    // connection. This distinguishes propagated pressure from an idle source.
    let before = wire.bytes;
    tokio::time::timeout(Duration::from_secs(5), async {
        while probe.snapshot().written == held.written {
            ensure!(
                wire.bytes - before < MAX_RESUME,
                "reader resume exhausted byte budget"
            );
            ensure!(
                wire.read(&mut reader).await?,
                "reader resume found a closed stream"
            );
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("reading did not restore provider TCP write progress")??;
    ensure!(wire.bytes > before, "progress preceded the reader resume");
    let second = probe.stalled(held.written + 1, held.blocks + 1).await?;
    reject_locally(w, "stalled-again").await?;
    ensure!(probe.snapshot().pending && !probe.snapshot().body_dropped);

    // No more reads, no client close, no provider completion/termination, and
    // no process signal until capacity returns. The request body is complete.
    let retained = tokio::time::timeout(
        Duration::from_millis(STALL_MS + 6000),
        recovery::wait_view(w, &id, unknown),
    )
    .await
    .context("write stall did not converge to public Unknown")??;
    ensure!(
        retained.attempts.len() == 1
            && retained.attempts[0].attempt_id == running.attempts[0].attempt_id
    );
    ensure!(matches!(
        retained.attempts[0].state,
        AttemptState::Unknown { .. }
    ));
    probe
        .wait(Duration::from_secs(3), |s| s.body_dropped)
        .await?;
    ensure!(!probe.snapshot().cap_hit && second.written > first.written);
    recovery::retained_capacity(w, provider, &retained, true).await?;
    progress_on_owner(w, provider, clients).await?;
    ensure!(provider.calls() == 3);

    // Only after the public state and fresh owner request prove release do we
    // drain buffered bytes. A timeout is never accepted as interruption proof.
    tokio::time::timeout(Duration::from_secs(8), async {
        while wire.read(&mut reader).await? {}
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("interrupted reader did not observe EOF/reset")??;
    ensure!(wire.held && !wire.terminal && (!wire.done || wire.error));
    ensure!(
        !wire.http_complete || wire.error,
        "stalled response claimed clean success framing"
    );
    drop(reader);
    // Unknown must not synthesize provider success, termination or usage. The
    // original independent operation ends only on this acknowledged command.
    let receipt = arrival.end(End::Terminate).await?;
    recovery::resolve(w, &retained, receipt).await?;
    recovery::progress(
        w,
        provider,
        clients,
        3,
        "account-b:mock",
        "account",
        "resolved-account",
    )
    .await?;
    ensure!(provider.calls() == 4 && !probe.snapshot().cap_hit);
    Ok(())
}

async fn reject_locally(w: &World, tag: &str) -> Result<()> {
    let wire = tokio::time::timeout(Duration::from_secs(1), async {
        let health = Wire::read(
            w.http
                .get(format!("http://{}/health", w.replicas[0]))
                .send()
                .await?,
        )
        .await?;
        health.expect_status(StatusCode::OK)?;
        Wire::read(
            recovery::request(w, 0, 3, "other:mock", tag, false)
                .send()
                .await?,
        )
        .await
    })
    .await
    .context("local overload queued beyond 1s")??;
    wire.expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
    ensure!(wire.header("retry-after")? == "1");
    // The 8-connection transport cap is not full: this rejection must pass
    // through HTTP admission while the single custody slot is occupied.
    ensure!(wire.headers.get("x-bitrouter-execution-id").is_none());
    Ok(())
}

async fn progress_on_owner(
    w: &World,
    provider: &mut Provider,
    clients: &mut recovery::Clients,
) -> Result<()> {
    clients.start(
        "after-stall",
        recovery::request(w, 0, 3, "other:mock", "after-stall", false),
    );
    let arrival = provider.arrival("after-stall", "other", false).await?;
    let id = arrival.execution.clone();
    arrival.end(End::Complete).await?;
    clients
        .finished("after-stall")
        .await?
        .success(&id, "after-stall", false)?;
    recovery::observed_success(&w.wait_state(1, 3, &id, succeeded).await?)?;
    Ok(())
}

async fn start_world(
    fixture: &mut Fixture,
    base: &str,
    expected: &mut ExitExpectations,
) -> Result<World> {
    let keys = std::array::from_fn(|_| crate::auth::keys::generate());
    seed(fixture, &seed_plan(base, &keys)).await?;
    let mut replicas = Vec::new();
    let mut children = Vec::new();
    let mut operators = Vec::new();
    for replica in 0..2 {
        let inference = available_address()?;
        let operator = (0..8)
            .map(|_| available_address())
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .find(|address| *address != inference)
            .context("distinct operator address missing")?;
        let child = spawn_replica(fixture, inference, operator, replica == 0).await?;
        expected.insert(child, ExpectedExit::CleanServe);
        fixture.child(child)?.ready().await?;
        replicas.push(inference);
        children.push(child);
        operators.push(OperatorClient::new(
            &format!("http://{operator}"),
            OPERATOR_TOKEN,
        )?);
    }
    Ok(World {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(40))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("replica pair missing"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("child pair missing"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("operator pair missing"))?,
        keys,
    })
}

async fn spawn_replica(
    fixture: &mut Fixture,
    address: SocketAddr,
    operator: SocketAddr,
    bounded: bool,
) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    let document = json!({
        "server":{"listen":address.to_string(),"log_level":"info"},
        "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
        "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
            "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,"transaction_timeout_ms":1000,
            "owner_lease_ms":30000,"heartbeat_ms":250,"worker_interval_ms":100,"drain_timeout_ms":2000,
            "max_connections":8,"local_max_in_flight":if bounded {1} else {8},"max_request_bytes":1024,
            "header_timeout_ms":2000,"request_timeout_ms":20000,"write_stall_timeout_ms":STALL_MS},
        "operator":{"listen":operator.to_string(),"credentials":[{"id":"fixture-operator",
            "token_env":"FIXTURE_OPERATOR_TOKEN","grants":["read","policy_write","resolve_execution"]}]}
    });
    tokio::fs::write(&config, serde_json::to_vec(&document)?).await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env("FIXTURE_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address,
    });
    Ok(index)
}

// Reuse the reviewed provider's HTTP entry counter, validation, operation
// ownership, explicit receipts and finish() checks. Only this child's server
// wraps streaming output and observes the first connection's real writes.
async fn start_provider(probe: Arc<Probe>) -> Result<Provider> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (sender, arrivals) = mpsc::channel(16);
    let state = StateData {
        arrivals: sender,
        calls: Arc::default(),
        stop: CancellationToken::new(),
        operations: Arc::new(Mutex::new(JoinSet::new())),
    };
    let app = Router::new()
        .fallback(flood_request)
        .with_state((state.clone(), probe.clone()));
    let stop = state.stop.clone();
    let mut roots = JoinSet::new();
    roots.spawn(async move {
        axum::serve(
            ProbeListener {
                inner: listener,
                probe: Some(probe),
            },
            app,
        )
        .with_graceful_shutdown(stop.cancelled_owned())
        .await
    });
    Ok(Provider {
        base,
        state,
        arrivals,
        roots,
    })
}

async fn flood_request(
    State((state, probe)): State<(StateData, Arc<Probe>)>,
    request: Request,
) -> Response {
    let response = super::request(State(state.clone()), request).await;
    if !response
        .headers()
        .get("content-type")
        .is_some_and(|value| value == "text/event-stream")
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _dropped = BodyDrop(probe.clone());
        let mut original = body.into_data_stream();
        if let Some(first) = original.next().await {
            yield first;
        }
        tokio::select! { () = probe.start.cancelled() => {}, () = state.stop.cancelled() => return }
        let frame = Bytes::from(format!("data: {}\n\n", json!({"id":"fixture-stream","object":"chat.completion.chunk","created":1,"model":"mock","choices":[{"index":0,"delta":{"content":"x".repeat(CHUNK_TEXT)},"finish_reason":null}]})));
        for _ in 0..MAX_FRAMES {
            if state.stop.is_cancelled() { return; }
            yield Ok::<_, axum::Error>(frame.clone());
        }
        // Reaching the finite cap is a test failure, never an artificial EOF
        // that could manufacture the expected Unknown/capacity release.
        probe.state.send_modify(|s| s.cap_hit = true);
        state.stop.cancelled().await;
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

#[derive(Clone, Copy, Debug, Default)]
struct Observed {
    written: usize,
    blocks: usize,
    pending: bool,
    pending_since: Option<Instant>,
    closed: bool,
    body_dropped: bool,
    cap_hit: bool,
}

struct Probe {
    state: watch::Sender<Observed>,
    start: CancellationToken,
}
impl Probe {
    fn new() -> Self {
        Self {
            state: watch::channel(Observed::default()).0,
            start: CancellationToken::new(),
        }
    }
    fn snapshot(&self) -> Observed {
        *self.state.borrow()
    }
    async fn stalled(&self, written: usize, blocks: usize) -> Result<Observed> {
        // Filter transient kernel Pending during initial fill. This measures
        // an actually observed blocked write, not absence of a provider call.
        tokio::time::timeout(Duration::from_secs(6), async {
            let mut receiver = self.state.subscribe();
            loop {
                let state = *receiver.borrow_and_update();
                ensure!(
                    !state.cap_hit && !state.closed && !state.body_dropped,
                    "provider ended/exhausted bytes before sustained socket pressure: {state:?}"
                );
                if state.pending && state.written >= written && state.blocks >= blocks {
                    let deadline = state
                        .pending_since
                        .context("Pending write had no timestamp")?
                        + Duration::from_millis(200);
                    if Instant::now() >= deadline {
                        return Ok(state);
                    }
                    tokio::select! {
                        changed = receiver.changed() => { changed?; },
                        () = tokio::time::sleep_until(deadline) => {},
                    }
                } else {
                    receiver.changed().await?;
                }
            }
        })
        .await
        .context("provider socket did not remain Pending within 6s")?
    }
    async fn wait(
        &self,
        budget: Duration,
        predicate: impl Fn(Observed) -> bool,
    ) -> Result<Observed> {
        let mut receiver = self.state.subscribe();
        tokio::time::timeout(budget, async {
            loop {
                let state = *receiver.borrow_and_update();
                ensure!(
                    !state.cap_hit,
                    "bounded provider exhausted its frame budget before proving pressure"
                );
                if predicate(state) {
                    return Ok(state);
                }
                receiver.changed().await?;
            }
        })
        .await
        .context("actual provider TCP backpressure/progress barrier timed out")?
    }
    fn write(&self, result: &Poll<io::Result<usize>>) {
        match result {
            Poll::Pending => self.state.send_modify(|s| {
                if !s.pending {
                    s.blocks += 1;
                    s.pending_since = Some(Instant::now());
                }
                s.pending = true;
            }),
            Poll::Ready(Ok(count)) if *count > 0 => self.state.send_modify(|s| {
                s.written = s.written.saturating_add(*count);
                s.pending = false;
                s.pending_since = None;
            }),
            _ => {}
        }
    }
}
struct BodyDrop(Arc<Probe>);
impl Drop for BodyDrop {
    fn drop(&mut self) {
        self.0.state.send_modify(|s| s.body_dropped = true);
    }
}

struct ProbeListener {
    inner: TcpListener,
    probe: Option<Arc<Probe>>,
}
impl axum::serve::Listener for ProbeListener {
    type Io = ProbeIo;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (inner, address) = axum::serve::Listener::accept(&mut self.inner).await;
        (
            ProbeIo {
                inner,
                probe: self.probe.take(),
            },
            address,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
struct ProbeIo {
    inner: TcpStream,
    probe: Option<Arc<Probe>>,
}
impl Drop for ProbeIo {
    fn drop(&mut self) {
        if let Some(probe) = &self.probe {
            probe.state.send_modify(|s| s.closed = true);
        }
    }
}
impl AsyncRead for ProbeIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl AsyncWrite for ProbeIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Tokio can yield for its cooperative budget before checking socket
        // readiness. Exclude that yield from the pressure measurement, only
        // for this delegated poll; the provider task remains cooperative.
        let write = std::future::poll_fn(|cx| Pin::new(&mut self.inner).poll_write(cx, bytes));
        let result = std::pin::pin!(tokio::task::unconstrained(write)).poll(cx);
        if let Some(probe) = &self.probe {
            probe.write(&result);
        }
        result
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let write =
            std::future::poll_fn(|cx| Pin::new(&mut self.inner).poll_write_vectored(cx, bytes));
        let result = std::pin::pin!(tokio::task::unconstrained(write)).poll(cx);
        if let Some(probe) = &self.probe {
            probe.write(&result);
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn probe_measures_socket_pressure_with_exhausted_task_budget() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let socket = TcpStream::connect(listener.local_addr()?).await?;
        let (_peer, _) = listener.accept().await?;
        socket.writable().await?;
        let probe = Arc::new(Probe::new());
        let mut io = ProbeIo {
            inner: socket,
            probe: Some(probe.clone()),
        };
        for vectored in [false, true] {
            while tokio::task::coop::has_budget_remaining() {
                tokio::task::consume_budget().await;
            }
            // Observe one poll directly: retrying Pending would restore the
            // task budget and hide a scheduler yield recorded as pressure.
            let result = std::future::poll_fn(|cx| {
                Poll::Ready(if vectored {
                    Pin::new(&mut io).poll_write_vectored(cx, &[IoSlice::new(b"x")])
                } else {
                    Pin::new(&mut io).poll_write(cx, b"x")
                })
            })
            .await;
            ensure!(
                matches!(result, Poll::Ready(Ok(1))),
                "writable socket yielded for task budget: vectored={vectored}, result={result:?}"
            );
        }
        let state = probe.snapshot();
        ensure!(
            state.written == 2 && state.blocks == 0 && !state.pending,
            "scheduler yielding was recorded as socket pressure: {state:?}"
        );
        Ok(())
    })
    .await
    .context("cooperative-budget probe regression timed out")?
}

async fn read_head(socket: &mut TcpStream) -> Result<Wire> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        ensure!(bytes.len() < 8192, "response head exceeded bound");
        bytes.push(socket.read_u8().await?);
    }
    let mut lines = std::str::from_utf8(&bytes)?.split("\r\n");
    let mut status = lines.next().context("missing status")?.split_whitespace();
    ensure!(status.next() == Some("HTTP/1.1"));
    let status = StatusCode::from_u16(status.next().context("missing status code")?.parse()?)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("malformed response header")?;
        headers.append(
            reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
            value.trim().parse()?,
        );
    }
    Ok(Wire {
        status,
        headers,
        body: Vec::new(),
    })
}

// Incremental chunk/SSE decoder. Retain one bounded frame, not the flood.
#[derive(Default)]
struct StreamWire {
    bytes: usize,
    chunk: Option<usize>,
    boundary: usize,
    line: Vec<u8>,
    event: Vec<u8>,
    zero: bool,
    http_complete: bool,
    held: bool,
    terminal: bool,
    done: bool,
    error: bool,
}
impl StreamWire {
    async fn read(&mut self, reader: &mut TcpStream) -> Result<bool> {
        let mut bytes = [0_u8; 8192];
        let count = match reader.read(&mut bytes).await {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        self.bytes = self
            .bytes
            .checked_add(count)
            .context("wire count overflow")?;
        ensure!(self.bytes <= MAX_WIRE, "wire byte budget exceeded");
        for byte in &bytes[..count] {
            self.byte(*byte)?;
        }
        Ok(count != 0)
    }
    fn byte(&mut self, byte: u8) -> Result<()> {
        ensure!(!self.http_complete, "bytes after HTTP chunk terminator");
        if self.boundary != 0 {
            ensure!(
                byte == if self.boundary == 2 { b'\r' } else { b'\n' },
                "invalid chunk boundary"
            );
            self.boundary -= 1;
            if self.boundary == 0 && self.zero {
                self.http_complete = true;
            }
            return Ok(());
        }
        if let Some(remaining) = self.chunk.as_mut() {
            *remaining -= 1;
            let complete = *remaining == 0;
            self.event.push(byte);
            ensure!(self.event.len() <= 128 * 1024, "SSE event exceeded bound");
            if self.event.ends_with(b"\n\n") || self.event.ends_with(b"\r\n\r\n") {
                self.event()?;
                self.event.clear();
            }
            if complete {
                self.chunk = None;
                self.boundary = 2;
            }
            return Ok(());
        }
        self.line.push(byte);
        ensure!(self.line.len() <= 128, "chunk size line exceeded bound");
        if self.line.ends_with(b"\r\n") {
            let line = std::str::from_utf8(&self.line[..self.line.len() - 2])?;
            let size =
                usize::from_str_radix(line.split(';').next().context("missing chunk length")?, 16)?;
            ensure!(size <= MAX_WIRE, "chunk exceeded wire budget");
            self.line.clear();
            if size == 0 {
                self.zero = true;
                self.boundary = 2;
            } else {
                self.chunk = Some(size);
            }
        }
        Ok(())
    }
    fn event(&mut self) -> Result<()> {
        let text = std::str::from_utf8(&self.event)?;
        let data = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(&data)?;
        self.error |= value.get("error").is_some_and(Value::is_object);
        if let Some(choices) = value["choices"].as_array() {
            for choice in choices {
                self.held |= choice["delta"]["content"].as_str() == Some("held");
                self.terminal |= choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null());
            }
        }
        Ok(())
    }
}

// Part of the single process test, before fixture allocation. Check that its
// bounded wire oracle distinguishes framing EOF and incomplete SSE terminals.
fn check_wire_decoder() -> Result<()> {
    let event =
        "data: {\"choices\":[{\"delta\":{\"content\":\"held\"},\"finish_reason\":null}]}\n\n";
    let complete = format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len());
    let mut wire = StreamWire::default();
    for byte in complete.bytes() {
        wire.byte(byte)?;
    }
    ensure!(wire.http_complete && wire.held && !wire.terminal && !wire.done);
    ensure!(wire.byte(b'x').is_err());
    let mut truncated = StreamWire::default();
    for byte in complete[..complete.len() - 3].bytes() {
        truncated.byte(byte)?;
    }
    ensure!(truncated.held && !truncated.http_complete);
    let partial = "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}";
    let framed = format!("{:x}\r\n{partial}\r\n", partial.len());
    let mut partial_event = StreamWire::default();
    for byte in framed.bytes() {
        partial_event.byte(byte)?;
    }
    ensure!(!partial_event.terminal && !partial_event.http_complete);
    Ok(())
}
