use super::*;

async fn yields() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn t2_graceful_shutdown_preserves_blocked_final_response() -> Result<()> {
    for vectored in [false, true] {
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = dropped.clone();
        let payload = format!("{{\"value\":\"{}\"}}", "a".repeat(1024));
        let expected = payload.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                let dropped = dropped.clone();
                let payload = payload.clone();
                async move { frames([Frame::data(Bytes::from(payload))], dropped) }
            }),
        );
        let (server, mut peer) = tokio::io::duplex(64);
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned()?;
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            connection(
                deadlines::Vectored {
                    inner: server,
                    enabled: vectored,
                },
                router,
                limits(),
                stopping,
            )
            .await;
        });
        peer.write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await?;
        yields().await;
        ensure!(observed.load(Ordering::SeqCst) && !task.is_finished());
        stop.cancel();
        yields().await;
        ensure!(
            !task.is_finished() && slots.available_permits() == 0,
            "graceful shutdown released unflushed response"
        );
        let mut wire = Vec::new();
        peer.read_to_end(&mut wire).await?;
        task.await?;
        ensure!(
            wire.starts_with(b"HTTP/1.1 200") && wire.ends_with(expected.as_bytes()),
            "graceful shutdown truncated response"
        );
        ensure!(slots.available_permits() == 1);
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn t2_early_response_survives_late_unread_body_and_backpressure() -> Result<()> {
    for vectored in [false, true] {
        for resumes in [false, true] {
            let payload = "x".repeat(6000);
            let expected = payload.clone();
            let router = Router::new().route(
                "/",
                post(move || {
                    let payload = payload.clone();
                    async move { (StatusCode::UNAUTHORIZED, payload) }
                }),
            );
            let (server, mut peer) = tokio::io::duplex(128);
            let slots = Arc::new(Semaphore::new(1));
            let permit = slots.clone().try_acquire_owned()?;
            let task = tokio::spawn(async move {
                let _permit = permit;
                connection(
                    deadlines::Vectored {
                        inner: server,
                        enabled: vectored,
                    },
                    router,
                    limits(),
                    CancellationToken::new(),
                )
                .await;
            });
            peer.write_all(b"POST / HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\n\r\n")
                .await?;
            yields().await;
            ensure!(
                !task.is_finished(),
                "early response closed before a write deadline"
            );
            // Wake the read side after the response body has been encoded.
            // One byte leaves the unused request body deliberately unfinished.
            peer.write_all(b"x").await?;
            yields().await;
            ensure!(
                !task.is_finished() && slots.available_permits() == 0,
                "late unread body truncated blocked response"
            );
            if resumes {
                let mut wire = Vec::new();
                peer.read_to_end(&mut wire).await?;
                task.await?;
                ensure!(
                    wire.starts_with(b"HTTP/1.1 401") && wire.ends_with(expected.as_bytes()),
                    "early response was truncated"
                );
            } else {
                tokio::time::advance(Duration::from_secs(1)).await;
                ensure!(!task.is_finished());
                tokio::time::advance(Duration::from_secs(2)).await;
                task.await?;
            }
            ensure!(slots.available_permits() == 1);
        }
    }
    Ok(())
}
