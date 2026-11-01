use super::super::owned::Transport;
use super::*;

fn local_limits() -> Limits {
    Limits {
        connections: 2,
        requests: 1,
        reuse: true,
        ..limits()
    }
}

async fn start(router: Router, limits: Limits) -> Result<(Transport, std::net::SocketAddr)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    Ok((Transport::start(listener, router, limits)?, address))
}

async fn response(peer: &mut tokio::io::DuplexStream) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        ensure!(head.len() < 65_536, "fixture response head exceeded bound");
        head.push(peer.read_u8().await?);
    }
    let text = std::str::from_utf8(&head)?;
    let length: usize = text
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .context("fixture response has no length")?
        .parse()?;
    let mut body = vec![0; length];
    peer.read_exact(&mut body).await?;
    Ok(body)
}

#[tokio::test]
async fn configured_local_http1_reuses_connection_and_releases_requests() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut config = crate::config::tenancy::TenancyConfig::default();
        config.capacity.transport.connections = 1;
        config.capacity.ingress.requests = 1;
        let (owner, address) = start(
            Router::new().route("/", get(|| async { "reused" })),
            Limits::for_tenancy(&config),
        )
        .await?;
        let client = reqwest::Client::builder().http1_only().no_proxy().build()?;
        for _ in 0..4 {
            ensure!(
                client
                    .get(format!("http://{address}/"))
                    .send()
                    .await?
                    .text()
                    .await?
                    == "reused"
            );
        }
        ensure!(
            owner.report().connections.started == 1,
            "local keep-alive opened extra connections"
        );
        ensure!(owner.drain().await?.is_clean());
        ensure!(owner.available_requests() == 1);
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn local_http1_request_limit_precedes_handler_and_recovers() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let release = Arc::new(Notify::new());
        let resume = release.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let router = Router::new()
            .route(
                "/hold",
                get(move || {
                    let signal = signal.clone();
                    let resume = resume.clone();
                    async move {
                        signal.notify_one();
                        resume.notified().await;
                        "released"
                    }
                }),
            )
            .route(
                "/ok",
                get(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    async { "ok" }
                }),
            );
        let (owner, address) = start(router, local_limits()).await?;
        let client = reqwest::Client::builder().http1_only().no_proxy().build()?;
        let pending = tokio::spawn(client.get(format!("http://{address}/hold")).send());
        entered.notified().await;
        ensure!(owner.available_requests() == 0);
        let rejected = client.get(format!("http://{address}/ok")).send().await?;
        ensure!(
            rejected.status() == StatusCode::SERVICE_UNAVAILABLE
                && calls.load(Ordering::SeqCst) == 0
        );
        ensure!(rejected.headers()[http::header::CONNECTION] == "close");
        release.notify_one();
        ensure!(pending.await??.text().await? == "released");
        while owner.available_requests() == 0 {
            tokio::task::yield_now().await;
        }
        ensure!(
            client
                .get(format!("http://{address}/ok"))
                .send()
                .await?
                .text()
                .await?
                == "ok"
        );
        ensure!(calls.load(Ordering::SeqCst) == 1);
        ensure!(owner.drain().await?.is_clean());
        Ok(())
    })
    .await?
}

#[tokio::test(start_paused = true)]
async fn local_pipeline_waits_for_final_flush_and_keeps_aggregate_permit() -> Result<()> {
    for vectored in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = dropped.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let dropped = dropped.clone();
                async move { frames([Frame::data(Bytes::from(vec![b'x'; 1024]))], dropped) }
            }),
        );
        let (server, mut peer) = tokio::io::duplex(256);
        let slots = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(serve_http1(
            deadlines::Vectored {
                inner: server,
                enabled: vectored,
            },
            router,
            local_limits(),
            CancellationToken::new(),
            slots.clone(),
            Instant::now() + Duration::from_secs(2),
        ));
        let input = b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\nGET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n";
        peer.write_all(input).await?;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        ensure!(
            observed.load(Ordering::SeqCst),
            "final source frame was not consumed"
        );
        ensure!(
            slots.available_permits() == 0 && calls.load(Ordering::SeqCst) == 1,
            "pipelined service or permit escaped an unflushed response"
        );
        let mut wire = Vec::new();
        peer.read_to_end(&mut wire).await?;
        task.await?;
        ensure!(calls.load(Ordering::SeqCst) == 2 && slots.available_permits() == 1);
        ensure!(
            wire.windows(b"HTTP/1.1 200".len())
                .filter(|w| *w == b"HTTP/1.1 200")
                .count()
                == 2
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reused_http1_rearms_body_and_header_deadlines() -> Result<()> {
    for partial in [
        b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\n\r\nx".as_slice(),
        b"GET / HTTP/1.1\r\nHost: fixture\r\n".as_slice(),
    ] {
        let router =
            Router::new().route("/", get(|| async { "ok" }).post(|_: Bytes| async { "ok" }));
        let (server, mut peer) = tokio::io::duplex(1024);
        let slots = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(serve_http1(
            server,
            router,
            local_limits(),
            CancellationToken::new(),
            slots.clone(),
            Instant::now() + Duration::from_secs(2),
        ));
        peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        ensure!(response(&mut peer).await? == b"ok");
        peer.write_all(partial).await?;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(3)).await;
        task.await?;
        ensure!(
            slots.available_permits() == 1,
            "expired reused request retained its permit"
        );
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn ignored_upload_closes_before_a_pipelined_request_and_releases_capacity() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let router = Router::new().route(
        "/",
        post(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            async { "early" }
        }),
    );
    let (server, mut peer) = tokio::io::duplex(1024);
    let slots = Arc::new(Semaphore::new(1));
    let task = tokio::spawn(serve_http1(
        server,
        router,
        local_limits(),
        CancellationToken::new(),
        slots.clone(),
        Instant::now() + Duration::from_secs(2),
    ));
    peer.write_all(b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 4\r\n\r\njunkPOST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 0\r\n\r\n").await?;
    let mut wire = Vec::new();
    peer.read_to_end(&mut wire).await?;
    task.await?;
    ensure!(calls.load(Ordering::SeqCst) == 1 && slots.available_permits() == 1);
    ensure!(std::str::from_utf8(&wire)?.contains("connection: close"));
    Ok(())
}

#[tokio::test]
async fn local_connection_limit_precedes_headers_and_deadline_reclaims_slot() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (owner, address) = start(
            Router::new().route("/", get(|| async { "ok" })),
            Limits {
                connections: 1,
                header_timeout: Duration::from_millis(150),
                ..local_limits()
            },
        )
        .await?;
        let mut partial = tokio::net::TcpStream::connect(address).await?;
        partial.write_all(b"GET / HTTP/1.1\r\n").await?;
        while owner.report().connections.started == 0 {
            tokio::task::yield_now().await;
        }
        let mut excess = tokio::net::TcpStream::connect(address).await?;
        ensure!(excess.read(&mut [0; 1]).await? == 0);
        let mut rejected = Vec::new();
        partial.read_to_end(&mut rejected).await?;
        while owner.report().connections.pending != 0 {
            tokio::task::yield_now().await;
        }
        let client = reqwest::Client::builder().http1_only().no_proxy().build()?;
        ensure!(
            client
                .get(format!("http://{address}/"))
                .send()
                .await?
                .text()
                .await?
                == "ok"
        );
        ensure!(owner.drain().await?.is_clean());
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn local_cancelled_drain_then_force_joins_handler_and_request_permit() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = dropped.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                let entered = entered.clone();
                let dropped = dropped.clone();
                async move {
                    let _body = frames([], dropped);
                    entered.notify_one();
                    std::future::pending::<()>().await;
                    "unreachable"
                }
            }),
        );
        let (owner, address) = start(router, local_limits()).await?;
        let mut peer = tokio::net::TcpStream::connect(address).await?;
        peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        signal.notified().await;
        let mut drain = Box::pin(owner.drain());
        ensure!(futures::poll!(drain.as_mut()).is_pending());
        drop(drain);
        ensure!(!observed.load(Ordering::SeqCst) && owner.available_requests() == 0);
        owner.request_force();
        ensure!(owner.join_forced().await?.is_quiescent());
        ensure!(observed.load(Ordering::SeqCst) && owner.available_requests() == 1);
        Ok(())
    })
    .await?
}
