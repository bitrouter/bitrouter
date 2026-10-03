//! Bound managed provider ingress before buffering or decoding. Reqwest yields
//! decoded entity chunks, so Content-Length is only an early rejection hint.
//! <https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.bytes_stream>

use futures::{Stream, StreamExt};

use super::{PipelineContext, upstream_body_error};
use crate::error::{BitrouterError, Result};
use crate::language_model::native::NativeResponseByteLimit;

pub(super) fn limit(ctx: &PipelineContext) -> Option<u64> {
    ctx.extension::<NativeResponseByteLimit>()
        .map(|limit| limit.0)
}

fn exceeded() -> BitrouterError {
    BitrouterError::UpstreamInvalidResponse {
        message: "managed upstream response exceeds byte limit".into(),
    }
}

pub(super) fn check_length(response: &reqwest::Response, limit: Option<u64>) -> Result<()> {
    if limit.is_some_and(|limit| {
        response
            .content_length()
            .is_some_and(|length| length > limit)
    }) {
        return Err(exceeded());
    }
    Ok(())
}

pub(super) fn bounded<S, B>(source: S, limit: Option<u64>) -> impl Stream<Item = Result<B>>
where
    S: Stream<Item = std::result::Result<B, reqwest::Error>>,
    B: AsRef<[u8]>,
{
    async_stream::try_stream! {
        futures::pin_mut!(source);
        let mut consumed = 0_u64;
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(|error| upstream_body_error("reading upstream body", error))?;
            if let Some(limit) = limit {
                consumed = consumed.checked_add(chunk.as_ref().len() as u64).ok_or_else(exceeded)?;
                if consumed > limit {
                    Err(exceeded())?;
                }
            }
            yield chunk;
        }
    }
}

pub(super) async fn read(response: reqwest::Response, ctx: &PipelineContext) -> Result<String> {
    let Some(limit) = limit(ctx) else {
        return response
            .text()
            .await
            .map_err(|error| upstream_body_error("reading upstream body", error));
    };
    let successful = response.status().is_success();
    match read_bounded(response, limit).await {
        // The HTTP status still governs rejection, refresh and Retry-After.
        // Body admission must not turn a known 4xx into a fallback-eligible
        // invalid successful response. Never retain the rejected body itself.
        Err(BitrouterError::UpstreamInvalidResponse { .. }) if !successful => {
            Ok("managed upstream error body unavailable".into())
        }
        result => result,
    }
}

async fn read_bounded(response: reqwest::Response, limit: u64) -> Result<String> {
    check_length(&response, Some(limit))?;
    let source = bounded(response.bytes_stream(), Some(limit));
    futures::pin_mut!(source);
    let mut bytes = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk?;
        // Avoid geometric Vec growth past the admitted body length.
        bytes
            .try_reserve_exact(chunk.len())
            .map_err(|_| BitrouterError::internal("managed upstream response allocation failed"))?;
        bytes.extend_from_slice(&chunk);
    }
    // JSON/SSE are UTF-8. Reject invalid bytes instead of lossily expanding them
    // after admission. Ordinary inference retains reqwest's text decoding path.
    // <https://www.rfc-editor.org/rfc/rfc8259#section-8.1>
    String::from_utf8(bytes).map_err(|_| BitrouterError::UpstreamInvalidResponse {
        message: "managed upstream response is not UTF-8".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Eventsource;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn stops_before_yielding_an_oversized_chunk_or_polling_further() -> Result<()> {
        let polled = Arc::new(AtomicUsize::new(0));
        let counter = polled.clone();
        let source = futures::stream::iter([b"1234".to_vec(), b"56".to_vec(), b"7".to_vec()]).map(
            move |chunk| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<_, reqwest::Error>(chunk)
            },
        );
        let bounded = bounded(source, Some(5));
        futures::pin_mut!(bounded);
        assert_eq!(bounded.next().await.transpose()?, Some(b"1234".to_vec()));
        assert!(matches!(
            bounded.next().await,
            Some(Err(BitrouterError::UpstreamInvalidResponse { .. }))
        ));
        assert!(bounded.next().await.is_none());
        assert_eq!(polled.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn exact_limit_and_unlimited_streams_preserve_the_original_bytes() -> Result<()> {
        for limit in [None, Some(6)] {
            let source = futures::stream::iter([Ok(b"1234".to_vec()), Ok(b"56".to_vec())]);
            let received = bounded(source, limit)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(received.concat(), b"123456");
        }
        let source = futures::stream::iter([Ok(b"x".to_vec())]);
        let received = bounded(source, Some(0)).collect::<Vec<_>>().await;
        assert!(matches!(
            received.as_slice(),
            [Err(BitrouterError::UpstreamInvalidResponse { .. })]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn unfinished_sse_events_cannot_accumulate_beyond_the_bound() {
        let source = futures::stream::iter([
            Ok(b"data: ".to_vec()),
            Ok(vec![b'x'; 128]),
            Ok(b"\n\n".to_vec()),
        ]);
        let events = bounded(source, Some(64)).eventsource();
        futures::pin_mut!(events);
        assert!(matches!(
            events.next().await,
            Some(Err(eventsource_stream::EventStreamError::Transport(
                BitrouterError::UpstreamInvalidResponse { .. }
            )))
        ));
        assert!(events.next().await.is_none());
    }
}
