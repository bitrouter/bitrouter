//! An optional relative deadline, explicitly relayed only to configured gateways.

use super::{context::PipelineContext, executor::StreamPartStream};
use crate::gateway::error::{BitrouterError, Result};
use futures::Stream;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time::{Duration, Instant, Sleep};

pub(crate) const HEADER: &str = "x-bitrouter-timeout-ms";

pub(crate) struct Deadline(pub(crate) Instant);

fn milliseconds(value: &http::HeaderValue) -> Option<u32> {
    value
        .to_str()
        .ok()
        .and_then(|text| {
            (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| text.parse::<u32>().ok())
                .flatten()
        })
        .filter(|ms| (1..=3_600_000).contains(ms))
}

pub(crate) fn parse(headers: &http::HeaderMap) -> Result<Option<Deadline>> {
    let mut values = headers.get_all(HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let milliseconds = milliseconds(value);
    if values.next().is_some() || milliseconds.is_none() {
        return Err(BitrouterError::bad_request(
            "invalid x-bitrouter-timeout-ms",
        ));
    }
    Ok(milliseconds.map(|ms| Deadline(Instant::now() + Duration::from_millis(u64::from(ms)))))
}

pub(crate) fn at(ctx: &PipelineContext) -> Option<Instant> {
    let deadline = ctx.extension::<Deadline>().map(|value| value.0);
    #[cfg(feature = "inference-routing")]
    let deadline = match ctx.extension::<crate::gateway::scheduling::bridge::Prepared>() {
        Some(prepared) => Some(deadline.map_or(prepared.deadline, |at| at.min(prepared.deadline))),
        None => deadline,
    };
    deadline
}

/// A configured default is a fallback, not a cap on an explicit inbound value.
pub(crate) fn relay(
    ctx: &PipelineContext,
    default: Option<&http::HeaderValue>,
) -> Option<http::HeaderValue> {
    let remaining = at(ctx)?
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1);
    let remaining = if !ctx.headers().contains_key(HEADER)
        && let Some(default) = default
    {
        match milliseconds(default) {
            Some(ms) => remaining.min(u128::from(ms)),
            // Preserve generic provider-header validation/forwarding semantics.
            None => return Some(default.clone()),
        }
    } else {
        remaining
    };
    http::HeaderValue::from_str(&remaining.to_string()).ok()
}

pub(crate) async fn bounded<T>(
    ctx: &PipelineContext,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    match at(ctx) {
        Some(at) => {
            if Instant::now() >= at {
                return Err(BitrouterError::UpstreamTimeout);
            }
            tokio::time::timeout_at(at, future)
                .await
                .map_err(|_| BitrouterError::UpstreamTimeout)?
        }
        None => future.await,
    }
}

pub(crate) fn bind_stream(ctx: &PipelineContext, stream: StreamPartStream) -> StreamPartStream {
    match at(ctx) {
        Some(at) => Box::pin(TimedStream {
            inner: Some(stream),
            timer: Box::pin(tokio::time::sleep_until(at)),
        }),
        None => stream,
    }
}

struct TimedStream {
    inner: Option<StreamPartStream>,
    timer: Pin<Box<Sleep>>,
}
impl Stream for TimedStream {
    type Item = Result<bitrouter_ai::types::StreamPart>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = &mut this.inner else {
            return Poll::Ready(None);
        };
        if this.timer.as_mut().poll(cx).is_ready() {
            this.inner.take();
            return Poll::Ready(Some(Err(BitrouterError::UpstreamTimeout)));
        }
        let result = inner.as_mut().poll_next(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            this.inner.take();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result, ensure};

    #[tokio::test(start_paused = true)]
    async fn header_cases_and_expired_future_never_start_work() -> Result<()> {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../docs/specs/service-api/inference-timeout.cases.json"
        ))?;
        for case in cases.as_array().context("deadline cases")? {
            let mut headers = http::HeaderMap::new();
            for value in case["values"].as_array().context("header values")? {
                headers.append(
                    HEADER,
                    http::HeaderValue::from_str(value.as_str().context("header value")?)?,
                );
            }
            ensure!(
                parse(&headers).is_ok() == case["valid"].as_bool().context("valid expectation")?,
                "{case}"
            );
        }
        let prompt = serde_json::from_value(
            serde_json::json!({"model":"fixture","messages":[],"stream":false}),
        )?;
        let mut ctx = PipelineContext::new(super::super::types::PipelineRequest::new(
            "fixture",
            crate::gateway::caller::CallerContext::local(),
            prompt,
        ));
        ctx.insert_extension(std::sync::Arc::new(Deadline(Instant::now())));
        let started = std::sync::atomic::AtomicBool::new(false);
        let outcome = bounded(&ctx, async {
            started.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;
        ensure!(matches!(outcome, Err(BitrouterError::UpstreamTimeout)));
        ensure!(!started.load(std::sync::atomic::Ordering::SeqCst));
        Ok(())
    }
}
