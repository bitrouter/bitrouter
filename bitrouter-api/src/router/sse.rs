use std::time::Duration;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

pub(crate) fn reply<S>(sse_stream: S) -> impl warp::Reply
where
    S: futures_core::TryStream<Ok = warp::sse::Event> + Send + Sync + 'static,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    warp::sse::reply(
        warp::sse::keep_alive()
            .interval(KEEP_ALIVE_INTERVAL)
            .stream(sse_stream),
    )
}
