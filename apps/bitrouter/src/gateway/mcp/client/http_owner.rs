//! Keep the actual rmcp HTTP worker owner across fallible client initialization.
use futures::future::BoxFuture;
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use std::{future::Future, io, sync::Arc, sync::Mutex, task::Poll};

type Http = StreamableHttpClientTransport<reqwest::Client>;

enum State {
    Open(Box<Http>),
    Closing(BoxFuture<'static, Result<(), String>>),
    Closed(Result<(), String>),
}

struct Shared {
    state: Mutex<State>,
    #[cfg(all(test, feature = "core", feature = "harness"))]
    closing: tokio::sync::Notify,
}

/// Both the outer MCP owner and its handshake/service facade retain this state.
/// Dropping a failed handshake cannot discard the worker's sole JoinHandle.
#[derive(Clone)]
pub(super) struct Owner(Arc<Shared>);

impl Owner {
    pub(super) fn new(transport: Http) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State::Open(Box::new(transport))),
            #[cfg(all(test, feature = "core", feature = "harness"))]
            closing: tokio::sync::Notify::new(),
        }))
    }

    pub(super) async fn join(&self) -> Result<(), String> {
        std::future::poll_fn(|cx| {
            let mut state = match self.0.state.lock() {
                Ok(state) => state,
                Err(_) => return Poll::Ready(Err("MCP HTTP worker owner poisoned".into())),
            };
            if matches!(&*state, State::Open(_)) {
                // WorkerTransport::close takes its real JoinHandle into the
                // returned future. Retain that SAME future across deadlines.
                // https://docs.rs/rmcp/3.3.0/src/rmcp/transport/worker.rs.html
                let prior = std::mem::replace(
                    &mut *state,
                    State::Closed(Err("MCP HTTP close was interrupted".into())),
                );
                if let State::Open(mut transport) = prior {
                    *state = State::Closing(Box::pin(async move {
                        transport
                            .close()
                            .await
                            .map_err(|_| "MCP HTTP worker did not join cleanly".into())
                    }));
                }
                #[cfg(all(test, feature = "core", feature = "harness"))]
                self.0.closing.notify_one();
            }
            match &mut *state {
                State::Closing(close) => match close.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        *state = State::Closed(result.clone());
                        Poll::Ready(result)
                    }
                },
                State::Closed(result) => Poll::Ready(result.clone()),
                State::Open(_) => Poll::Ready(Err("MCP HTTP close did not start".into())),
            }
        })
        .await
    }

    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    pub(super) fn observe(&self) -> Probe {
        Probe(Arc::downgrade(&self.0))
    }
}

impl Transport<RoleClient> for Owner {
    type Error = io::Error;

    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), io::Error>> + Send + 'static {
        // The concrete worker send contract is Send + 'static. Erase its
        // opaque future inside the short lock so no receiver borrow escapes
        // this scope. The future owns the SDK's queue/cancellation handles;
        // Shared keeps the same real worker and close future throughout.
        // https://docs.rs/rmcp/3.3.0/src/rmcp/transport/worker.rs.html
        let send =
            self.0.state.lock().map_err(|_| "owner poisoned").and_then(
                |mut state| match &mut *state {
                    State::Open(transport) => {
                        let send =
                            <Http as Transport<RoleClient>>::send(transport.as_mut(), message);
                        Ok(Box::pin(send)
                            as BoxFuture<
                                'static,
                                Result<(), <Http as Transport<RoleClient>>::Error>,
                            >)
                    }
                    _ => Err("worker is closing"),
                },
            );
        async move {
            send.map_err(io::Error::other)?
                .await
                .map_err(|_| io::Error::other("MCP HTTP send failed"))
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        // The pinned WorkerTransport receive is only mpsc::Receiver::recv,
        // whose cancellation is safe. Poll it briefly under the mutex; never
        // hold a guard while Pending or block concurrent owned send futures.
        // https://docs.rs/rmcp/3.3.0/src/rmcp/transport/worker.rs.html
        // https://docs.rs/tokio/latest/tokio/sync/mpsc/struct.Receiver.html#cancel-safety
        std::future::poll_fn(|cx| {
            let Ok(mut state) = self.0.state.lock() else {
                // join() reports poison as unconfirmed cleanup and retains it.
                return Poll::Ready(None);
            };
            match &mut *state {
                State::Open(transport) => {
                    let receive = transport.receive();
                    tokio::pin!(receive);
                    receive.as_mut().poll(cx)
                }
                _ => Poll::Ready(None),
            }
        })
        .await
    }

    async fn close(&mut self) -> Result<(), io::Error> {
        self.join().await.map_err(io::Error::other)
    }
}

// Weak inspection of the actual retained transport/close future. It owns no
// worker and cannot mark cleanup complete, cancel it, or drive a separate join.
#[cfg(all(test, unix, feature = "core", feature = "harness"))]
pub(super) struct Probe(std::sync::Weak<Shared>);

#[cfg(all(test, unix, feature = "core", feature = "harness"))]
impl Probe {
    #[cfg(unix)]
    pub(super) async fn closing(&self) -> Result<(), String> {
        let shared = self.0.upgrade().ok_or("HTTP worker owner was lost")?;
        loop {
            let notified = shared.closing.notified();
            if !matches!(
                &*shared.state.lock().map_err(|_| "HTTP owner poisoned")?,
                State::Open(_)
            ) {
                return Ok(());
            }
            notified.await;
        }
    }

    pub(super) fn joined(&self) -> Result<bool, String> {
        let shared = self.0.upgrade().ok_or("HTTP worker owner was lost")?;
        let state = shared.state.lock().map_err(|_| "HTTP owner poisoned")?;
        match &*state {
            State::Closed(result) => result.clone().map(|()| true),
            _ => Ok(false),
        }
    }
}
