use super::super::body::valid_metadata;
use super::*;
use http::{HeaderMap, HeaderValue};

struct Backing {
    bytes: Vec<u8>,
    dropped: Arc<AtomicBool>,
}

impl AsRef<[u8]> for Backing {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn copied_chunks_release_large_backing_and_preserve_hints_and_trailers() -> Result<()> {
    for small_slice in [false, true] {
        let allocation = Arc::new(AtomicBool::new(false));
        let source = Arc::new(AtomicBool::new(false));
        let mut data = Bytes::from_owner(Backing {
            bytes: vec![b'x'; 16 * 1024 * 1024 + 1],
            dropped: allocation.clone(),
        });
        if small_slice {
            // Replace the original owner rather than leaving an independent
            // fixture clone alive beside the narrow slice.
            data = data.slice(data.len() - 1..);
        }
        let length = data.len();
        let mut trailers = HeaderMap::new();
        trailers.insert("x-fixture", HeaderValue::from_static("complete"));
        let mut body = Output::new(frames(
            [Frame::data(data), Frame::trailers(trailers)],
            source.clone(),
        ));
        let mut copied = Vec::new();
        let mut total = 0;
        while total < length {
            ensure!(!body.is_end_stream());
            ensure!(body.size_hint().exact() == Some((length - total) as u64));
            let frame = next(&mut body)
                .await
                .context("missing data")?
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let data = frame
                .into_data()
                .map_err(|_| anyhow::anyhow!("premature trailers"))?;
            ensure!(data.len() <= CHUNK_BYTES);
            total += data.len();
            copied.push(data);
            ensure!(
                allocation.load(Ordering::SeqCst) == (total == length),
                "source backing lifetime differs from remainder"
            );
        }
        // Keeping every copied chunk alive must not pin the original 16 MiB.
        ensure!(allocation.load(Ordering::SeqCst));
        let trailers = next(&mut body)
            .await
            .context("missing trailers")?
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .into_trailers()
            .map_err(|_| anyhow::anyhow!("expected trailers"))?;
        ensure!(trailers["x-fixture"] == "complete");
        ensure!(next(&mut body).await.is_none());
        ensure!(body.is_end_stream() && body.size_hint().exact() == Some(0));
        ensure!(!source.load(Ordering::SeqCst));
        drop(body);
        ensure!(source.load(Ordering::SeqCst));
        ensure!(copied.iter().map(Bytes::len).sum::<usize>() == length);
    }
    Ok(())
}

#[tokio::test]
async fn pending_remainder_prevents_eos_and_drop_releases_source() -> Result<()> {
    let allocation = Arc::new(AtomicBool::new(false));
    let source = Arc::new(AtomicBool::new(false));
    let data = Bytes::from_owner(Backing {
        bytes: vec![b'x'; CHUNK_BYTES + 1],
        dropped: allocation.clone(),
    });
    let mut body = Output::new(frames([Frame::data(data)], source.clone()));
    let first = next(&mut body)
        .await
        .context("missing frame")?
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    ensure!(!body.is_end_stream() && body.size_hint().exact() == Some(1));
    ensure!(!allocation.load(Ordering::SeqCst));
    drop(body);
    ensure!(allocation.load(Ordering::SeqCst) && source.load(Ordering::SeqCst));
    ensure!(
        first
            .data_ref()
            .is_some_and(|data| data.len() == CHUNK_BYTES)
    );
    Ok(())
}

#[tokio::test]
async fn metadata_bounds_count_duplicates_and_reject_trailers() -> Result<()> {
    let mut headers = HeaderMap::new();
    for _ in 0..HEADER_VALUES - 4 {
        headers.append("x-fixture", HeaderValue::from_static("x"));
    }
    ensure!(valid_metadata(&headers));
    headers.append("x-fixture", HeaderValue::from_static("x"));
    ensure!(!valid_metadata(&headers));
    let mut body = Output::new(frames([Frame::trailers(headers.clone())], Arc::default()));
    ensure!(
        next(&mut body)
            .await
            .context("missing trailer error")?
            .is_err()
    );
    ensure!(body.is_end_stream() && next(&mut body).await.is_none());
    let mut response = StatusCode::OK.into_response();
    *response.headers_mut() = headers;
    ensure!(bounded_response(response).status() == StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-fixture",
        HeaderValue::from_bytes(&vec![b'x'; METADATA_BYTES - 512 - 13])?,
    );
    ensure!(valid_metadata(&headers));
    headers.append("x-fixture", HeaderValue::from_static("x"));
    ensure!(!valid_metadata(&headers));
    let mut response = StatusCode::OK.into_response();
    response
        .extensions_mut()
        .insert(hyper::ext::ReasonPhrase::from_static(b"custom"));
    ensure!(
        bounded_response(response)
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .is_none()
    );
    ensure!(
        bounded_response(StatusCode::SWITCHING_PROTOCOLS.into_response()).status()
            == StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn ingress_limits_bytes_and_trailers_without_resetting_deadline() -> Result<()> {
    let (sender, receiver) = watch::channel(IngressState::Headers);
    let mut body = Ingress::new(
        frames(
            [
                Frame::data(Bytes::from_static(b"ab")),
                Frame::data(Bytes::from_static(b"c")),
            ],
            Arc::default(),
        ),
        2,
        sender,
        Duration::from_secs(2),
    );
    ensure!(body.size_hint().upper() == Some(2));
    ensure!(
        next(&mut body)
            .await
            .context("missing ingress data")?
            .is_ok()
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    ensure!(
        next(&mut body)
            .await
            .context("missing size error")?
            .is_err()
    );
    let expiry = tokio::spawn(ingress_expired(receiver));
    tokio::time::advance(Duration::from_secs(1)).await;
    expiry.await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-fixture",
        HeaderValue::from_bytes(&vec![b'x'; METADATA_BYTES])?,
    );
    let (sender, _) = watch::channel(IngressState::Headers);
    let mut body = Ingress::new(
        frames([Frame::trailers(headers)], Arc::default()),
        2,
        sender,
        Duration::from_secs(2),
    );
    ensure!(
        next(&mut body)
            .await
            .context("missing trailers error")?
            .is_err()
    );
    Ok(())
}
