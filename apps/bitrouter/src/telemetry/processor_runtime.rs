//! Runtime adapter for the OpenTelemetry 0.32 async processors.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use opentelemetry_sdk::runtime::{Runtime, RuntimeChannel, TokioCurrentThread};

/// Supplies the immediate first tick expected by the OpenTelemetry 0.32 async
/// processors, then preserves every configured delay unchanged.
///
/// The SDK's interval stream waits before every yield, but its processors still
/// discard the first yield as though it were immediate. This adapter makes only
/// that discarded delay immediate. See the upstream interval implementation:
/// <https://docs.rs/opentelemetry_sdk/0.32.1/src/opentelemetry_sdk/runtime.rs.html#46-58>.
#[derive(Debug, Clone)]
pub(super) struct ProcessorRuntime {
    inner: TokioCurrentThread,
    initial_delay: Arc<AtomicBool>,
}

/// Prevents an already-queued export message from reaching the worker before
/// its ticker consumes the synthetic immediate delay.
pub(super) struct ProcessorReceiver<T: Debug + Send> {
    inner: <TokioCurrentThread as RuntimeChannel>::Receiver<T>,
    initial_delay: Arc<AtomicBool>,
}

impl<T: Debug + Send> Stream for ProcessorReceiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.initial_delay.load(Ordering::Acquire) {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl ProcessorRuntime {
    pub(super) fn new() -> Self {
        Self {
            inner: TokioCurrentThread,
            initial_delay: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Runtime for ProcessorRuntime {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.inner.spawn(future);
    }

    fn delay(&self, duration: Duration) -> impl Future<Output = ()> + Send + 'static {
        let inner = self.inner.clone();
        let is_initial = self.initial_delay.swap(false, Ordering::AcqRel);
        async move {
            if !is_initial {
                inner.delay(duration).await;
            }
        }
    }
}

impl RuntimeChannel for ProcessorRuntime {
    type Receiver<T: Debug + Send> = ProcessorReceiver<T>;
    type Sender<T: Debug + Send> = <TokioCurrentThread as RuntimeChannel>::Sender<T>;

    fn batch_message_channel<T: Debug + Send>(
        &self,
        capacity: usize,
    ) -> (Self::Sender<T>, Self::Receiver<T>) {
        let (sender, receiver) = self.inner.batch_message_channel(capacity);
        (
            sender,
            ProcessorReceiver {
                inner: receiver,
                initial_delay: Arc::clone(&self.initial_delay),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::stream;
    use opentelemetry_sdk::runtime::Runtime;
    use opentelemetry_sdk::runtime::RuntimeChannel;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn first_delay_is_immediate_and_later_delays_keep_the_full_cadence() {
        let runtime = ProcessorRuntime::new();
        let interval = Duration::from_secs(5);
        let started = tokio::time::Instant::now();

        runtime.delay(interval).await;
        assert_eq!(started.elapsed(), Duration::ZERO);

        runtime.delay(interval).await;
        assert_eq!(started.elapsed(), interval);

        runtime.delay(interval).await;
        assert_eq!(started.elapsed(), interval * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_messages_wait_until_the_ticker_consumes_the_initial_delay() {
        let runtime = ProcessorRuntime::new();
        let (sender, receiver) = runtime.batch_message_channel(1);
        sender.try_send(42_u8).expect("test channel has capacity");

        let ticker_runtime = runtime.clone();
        let ticker = stream::unfold((), move |()| {
            let ticker_runtime = ticker_runtime.clone();
            async move {
                ticker_runtime.delay(Duration::from_secs(5)).await;
                Some((0_u8, ()))
            }
        })
        .skip(1);
        let messages = stream::select(receiver, ticker);
        futures::pin_mut!(messages);
        let started = tokio::time::Instant::now();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), messages.next())
                .await
                .expect("queued message is repolled without waiting for the ticker"),
            Some(42)
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
