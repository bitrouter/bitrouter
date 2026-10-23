use std::error::Error as StdError;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use futures_util::FutureExt;
use http_body_util::Empty;
use hyper::{Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};

use super::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
const CAPACITY: usize = 64;

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Hyper's real HTTP upgrade over a bounded duplex transport lets this test
// force Pending deterministically, independently of OS TCP buffer sizes.
// https://docs.rs/hyper/latest/hyper/upgrade/index.html
async fn upgraded_pair(capacity: usize) -> TestResult<(reqwest::Upgraded, reqwest::Upgraded)> {
    let (client, server) = tokio::io::duplex(capacity);
    let (peer, receive_peer) = oneshot::channel();
    let peer = Arc::new(Mutex::new(Some(peer)));
    let server = tokio::spawn(async move {
        let service =
            hyper::service::service_fn(move |mut request: Request<hyper::body::Incoming>| {
                let peer = peer.clone();
                async move {
                    let upgrade = hyper::upgrade::on(&mut request);
                    let sender = peer
                        .lock()
                        .await
                        .take()
                        .ok_or_else(|| std::io::Error::other("duplicate fixture request"))?;
                    tokio::spawn(async move {
                        if let Ok(upgraded) = upgrade.await {
                            let _ = sender.send(reqwest::Upgraded::from(upgraded));
                        }
                    });
                    let mut response = Response::new(Empty::<bytes::Bytes>::new());
                    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                    response.headers_mut().insert(
                        header::CONNECTION,
                        header::HeaderValue::from_static("upgrade"),
                    );
                    response.headers_mut().insert(
                        header::UPGRADE,
                        header::HeaderValue::from_static("websocket"),
                    );
                    Ok::<_, std::io::Error>(response)
                }
            });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(server), service)
            .with_upgrades()
            .await
    });
    let (mut requests, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(client)).await?;
    let client = tokio::spawn(connection.with_upgrades());
    let mut request = Request::new(Empty::<bytes::Bytes>::new());
    request
        .headers_mut()
        .insert(header::HOST, header::HeaderValue::from_static("localhost"));
    request.headers_mut().insert(
        header::CONNECTION,
        header::HeaderValue::from_static("upgrade"),
    );
    request.headers_mut().insert(
        header::UPGRADE,
        header::HeaderValue::from_static("websocket"),
    );
    let response = requests.send_request(request).await?;
    let upgraded = hyper::upgrade::on(response).await?;
    let peer = receive_peer.await?;
    server.await??;
    client.await??;
    Ok((reqwest::Upgraded::from(upgraded), peer))
}

#[tokio::test]
async fn cancelled_backpressured_send_invalidates_both_halves_and_close() -> TestResult {
    let (upgraded, mut peer) = upgraded_pair(CAPACITY).await?;
    let config = WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(32768);
    let channel = channel(upgraded, config, 16384).await;
    let (mut sender, mut receiver) = channel.split();
    let mut reading = Box::pin(receiver.receive());
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    assert!(
        reading
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    // A pending reader could ordinarily flush a queued pong and any retained
    // outgoing frame. Keep the reader parked until the sending future is gone.
    peer.write_all(&[0x89, 0]).await?;
    let command = ClientMessage {
        version: 1,
        session_id: "session".into(),
        execution_epoch: 1,
        operation_id: "steer".into(),
        expected_state_revision: None,
        command: bitrouter_ai::core::protocol::Command::Steer {
            run_id: "run".into(),
            agent_turn_id: "turn".into(),
            text: "x".repeat(8192),
        },
    };
    let before_cancellation = wakes.0.load(Ordering::SeqCst);
    assert!(sender.send(&command).now_or_never().is_none());
    assert!(wakes.0.load(Ordering::SeqCst) > before_cancellation);
    assert!(matches!(
        sender.send(&command).now_or_never(),
        Some(Err(Error::CoreChannel(_)))
    ));
    assert!(matches!(
        sender.close().now_or_never(),
        Some(Err(Error::CoreChannel(_)))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), reading)
            .await?
            .is_err()
    );
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes)).await??;
    // Only the prefix accepted before cancellation can reach the peer. No
    // later send, close or receiver-driven control reply may finish that frame.
    assert!(bytes.len() <= CAPACITY);
    Ok(())
}

#[tokio::test]
async fn cancelled_backpressured_close_invalidates_the_pending_receiver() -> TestResult {
    let (upgraded, mut peer) = upgraded_pair(1).await?;
    let channel = channel(upgraded, WebSocketConfig::default(), 16384).await;
    let (mut sender, mut receiver) = channel.split();
    let mut reading = Box::pin(receiver.receive());
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    assert!(
        reading
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    // Even the masked close frame cannot fit in the unread one-byte buffer.
    assert!(sender.close().now_or_never().is_none());
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        sender.close().now_or_never(),
        Some(Err(Error::CoreChannel(_)))
    ));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), reading)
            .await?
            .is_err()
    );
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes)).await??;
    assert!(bytes.len() <= 1);
    Ok(())
}
