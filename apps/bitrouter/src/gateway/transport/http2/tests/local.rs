use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn local_http1_and_http2_share_request_capacity_and_reset_reclaims_it() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let router = Router::new()
            .route(
                "/hold",
                post(move || {
                    let entered = entered.clone();
                    async move {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        "unreachable"
                    }
                }),
            )
            .route(
                "/ok",
                axum::routing::get(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    async { "ok" }
                })
                .post(|| async { "ok" }),
            );
        let (owner, address) = start(
            router,
            Limits {
                reuse: true,
                requests: 1,
                ..limits()
            },
        )
        .await?;
        let mut first = Client::connect(address).await?;
        let (_pending, mut cancel) = first.request("/hold", true).await?;
        signal.notified().await;
        ensure!(owner.available_requests() == 0);
        let mut h1 = TcpStream::connect(address).await?;
        h1.write_all(b"GET /ok HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        let mut rejected = Vec::new();
        h1.read_to_end(&mut rejected).await?;
        ensure!(rejected.starts_with(b"HTTP/1.1 503") && calls.load(Ordering::SeqCst) == 0);
        let mut second = Client::connect(address).await?;
        ensure!(
            second.request("/ok", true).await?.0.await?.status() == StatusCode::SERVICE_UNAVAILABLE
        );
        cancel.send_reset(Reason::CANCEL);
        while owner.available_requests() == 0 {
            tokio::task::yield_now().await;
        }
        ensure!(collect(second.request("/ok", true).await?.0.await?.into_body()).await? == "ok");
        first.close().await?;
        second.close().await?;
        stop(owner).await
    })
    .await?
}

#[tokio::test]
async fn large_host_request_budget_keeps_intrinsic_stream_limit_without_narrowing_wrap()
-> Result<()> {
    let Some(requests) = (u32::MAX as usize).checked_add(1) else {
        return Ok(());
    };
    timeout(Duration::from_secs(10), async {
        let (owner, address) = start(
            Router::new().route("/", post(|| async { "ok" })),
            Limits {
                reuse: true,
                requests,
                ..limits()
            },
        )
        .await?;
        let mut client = Client::connect(address).await?;
        ensure!(collect(client.request("/", true).await?.0.await?.into_body()).await? == "ok");
        client.close().await?;
        stop(owner).await
    })
    .await?
}

#[tokio::test]
async fn local_http2_connections_are_bounded_before_handshake() -> Result<()> {
    timeout(Duration::from_secs(10), async {
        let (owner, address) = start(
            Router::new().route("/", post(|| async { "ok" })),
            Limits {
                reuse: true,
                connections: 1,
                ..limits()
            },
        )
        .await?;
        let mut client = Client::connect(address).await?;
        ensure!(collect(client.request("/", true).await?.0.await?.into_body()).await? == "ok");
        let mut excess = TcpStream::connect(address).await?;
        ensure!(excess.read(&mut [0; 1]).await? == 0);
        ensure!(owner.report().connections.started == 1);
        client.close().await?;
        stop(owner).await
    })
    .await?
}
