#![cfg(feature = "core")]

use std::error::Error as StdError;
use std::time::Duration;

use bitrouter_ai::core::protocol::{ClientMessage, Command, ServerMessage, VERSION};
use bitrouter_sdk::core::ChannelOptions;
use bitrouter_sdk::error::Error;
use bitrouter_sdk::gateway::GatewayClient;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Message, handshake::derive_accept_key};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;

async fn peer() -> TestResult<(TcpListener, GatewayClient)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let client = GatewayClient::new(
        &format!("http://{}/prefix/v1", listener.local_addr()?),
        Some("fixture-key"),
    )?;
    Ok((listener, client))
}

async fn accept(listener: TcpListener) -> TestResult<WebSocketStream<TcpStream>> {
    let (stream, _) = listener.accept().await?;
    Ok(tokio_tungstenite::accept_hdr_async(stream, CheckHandshake).await?)
}

struct CheckHandshake;

impl tokio_tungstenite::tungstenite::handshake::server::Callback for CheckHandshake {
    fn on_request(
        self,
        request: &tokio_tungstenite::tungstenite::handshake::server::Request,
        mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        assert_eq!(request.uri().path(), "/prefix/v1/orchestrator/channel");
        assert_eq!(request.headers()["authorization"], "Bearer fixture-key");
        assert_eq!(request.headers()["bitrouter-beta"], "orchestrator_core=v1");
        response.headers_mut().insert(
            "connection",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static("keep-alive, Upgrade"),
        );
        Ok(response)
    }
}

fn command() -> ClientMessage {
    ClientMessage {
        version: VERSION,
        session_id: "session".into(),
        execution_epoch: 1,
        operation_id: "inspect".into(),
        expected_state_revision: None,
        command: Command::Operation {
            target_operation_id: "prior".into(),
        },
    }
}

fn event() -> TestResult<Message> {
    Ok(Message::Text(
        serde_json::to_string(&ServerMessage::UnknownOperation {
            operation_id: "prior".into(),
        })?
        .into(),
    ))
}

async fn finish(mut socket: WebSocketStream<TcpStream>) -> TestResult {
    socket.close(None).await?;
    // Drive the server's close reply after the client acknowledges it.
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.next()).await?;
    Ok(())
}

#[tokio::test]
async fn explicit_messages_preserve_prefix_auth_beta_and_close_semantics() -> TestResult {
    let (listener, client) = peer().await?;
    let (checked, no_automatic_command) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), socket.next())
                .await
                .is_err()
        );
        checked.send(()).map_err(|_| "client gone")?;
        let frame = socket.next().await.ok_or("missing explicit command")??;
        let decoded: ClientMessage = serde_json::from_str(frame.to_text()?)?;
        assert!(matches!(decoded.command, Command::Operation { .. }));
        socket.send(event()?).await?;
        finish(socket).await
    });
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    no_automatic_command.await?;
    channel.send(&command()).await?;
    assert!(
        matches!(channel.receive().await?, Some(ServerMessage::UnknownOperation { operation_id }) if operation_id == "prior")
    );
    assert!(channel.receive().await?.is_none());
    assert!(matches!(
        channel.send(&command()).await,
        Err(Error::CoreChannel(_))
    ));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn client_initiated_close_completes_the_peer_handshake() -> TestResult {
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        assert!(matches!(socket.next().await, Some(Ok(Message::Close(_)))));
        // Poll again to flush tungstenite's queued close acknowledgement.
        assert!(
            tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await?
                .is_none()
        );
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    channel.close().await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), channel.receive())
            .await??
            .is_none()
    );
    assert!(matches!(
        channel.send(&command()).await,
        Err(Error::CoreChannel(_))
    ));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn split_receive_allows_sending_and_drives_automatic_pong() -> TestResult {
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        socket.send(Message::Ping(b"probe".to_vec().into())).await?;
        let mut command_seen = false;
        let mut pong_seen = false;
        while !command_seen || !pong_seen {
            match socket.next().await.ok_or("client disconnected")?? {
                Message::Text(text) => {
                    let _: ClientMessage = serde_json::from_str(&text)?;
                    command_seen = true;
                }
                Message::Pong(payload) => {
                    assert_eq!(&payload[..], b"probe");
                    pong_seen = true;
                }
                _ => return Err("unexpected frame".into()),
            }
        }
        socket.send(event()?).await?;
        finish(socket).await
    });
    let channel = client.core_channel(ChannelOptions::default()).await?;
    let (mut sender, mut receiver) = channel.split();
    let reading = tokio::spawn(async move {
        assert!(matches!(
            receiver.receive().await?,
            Some(ServerMessage::UnknownOperation { .. })
        ));
        assert!(receiver.receive().await?.is_none());
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    sender.send(&command()).await?;
    tokio::time::timeout(Duration::from_secs(3), reading).await???;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn malformed_data_ends_reader_and_rejects_later_sends_without_echoing_payload() -> TestResult
{
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        socket
            .send(Message::Text("fixture-key is not JSON".into()))
            .await?;
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    let error = channel
        .receive()
        .await
        .err()
        .ok_or("malformed data was accepted")?;
    assert!(matches!(error, Error::CoreChannel(_)));
    assert!(!format!("{error:?} {channel:?}").contains("fixture-key"));
    assert!(channel.receive().await?.is_none());
    assert!(matches!(
        channel.send(&command()).await,
        Err(Error::CoreChannel(_))
    ));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn incoming_message_limit_is_enforced_before_json_decoding() -> TestResult {
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        socket.send(Message::Text("x".repeat(8192).into())).await?;
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let mut channel = client
        .core_channel(ChannelOptions {
            max_message_bytes: 512,
        })
        .await?;
    assert!(matches!(channel.receive().await, Err(Error::Limit)));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn fragmented_messages_obey_the_total_message_limit() -> TestResult {
    use tokio_tungstenite::tungstenite::protocol::frame::{
        Frame,
        coding::{Data, OpCode},
    };
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        socket
            .send(Message::Frame(Frame::message(
                vec![b'x'; 300],
                OpCode::Data(Data::Text),
                false,
            )))
            .await?;
        socket
            .send(Message::Frame(Frame::message(
                vec![b'x'; 300],
                OpCode::Data(Data::Continue),
                false,
            )))
            .await?;
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let mut channel = client
        .core_channel(ChannelOptions {
            max_message_bytes: 512,
        })
        .await?;
    assert!(matches!(channel.receive().await, Err(Error::Limit)));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn abrupt_disconnect_is_an_error_and_does_not_reconnect() -> TestResult {
    let (listener, client) = peer().await?;
    let (drop_socket, dropped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let socket = accept(listener).await?;
        dropped.await?;
        drop(socket);
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let mut channel = client.core_channel(ChannelOptions::default()).await?;
    drop_socket.send(()).map_err(|_| "peer gone")?;
    assert!(matches!(
        channel.receive().await,
        Err(Error::CoreChannel(_))
    ));
    assert!(channel.receive().await?.is_none());
    assert!(matches!(
        channel.send(&command()).await,
        Err(Error::CoreChannel(_))
    ));
    server.await??;
    Ok(())
}

#[tokio::test]
async fn invalid_limits_do_not_open_a_connection() -> TestResult {
    let (listener, client) = peer().await?;
    for limit in [0, usize::MAX] {
        assert!(matches!(
            client
                .core_channel(ChannelOptions {
                    max_message_bytes: limit
                })
                .await,
            Err(Error::Options(_))
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn oversized_outbound_data_is_not_dispatched_and_valid_command_still_works() -> TestResult {
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let mut socket = accept(listener).await?;
        let frame = socket.next().await.ok_or("missing valid command")??;
        let decoded: ClientMessage = serde_json::from_str(frame.to_text()?)?;
        assert!(matches!(decoded.command, Command::Operation { .. }));
        socket.send(event()?).await?;
        finish(socket).await
    });
    let mut channel = client
        .core_channel(ChannelOptions {
            max_message_bytes: 512,
        })
        .await?;
    let mut large = command();
    large.command = Command::Steer {
        run_id: "run".into(),
        agent_turn_id: "turn".into(),
        text: "x".repeat(8192),
    };
    assert!(matches!(channel.send(&large).await, Err(Error::Limit)));
    channel.send(&command()).await?;
    assert!(channel.receive().await?.is_some());
    assert!(channel.receive().await?.is_none());
    server.await??;
    Ok(())
}

async fn request_headers(stream: &mut TcpStream) -> TestResult<String> {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        bytes.push(byte);
        if bytes.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8(bytes)?);
        }
        if bytes.len() > 16384 {
            return Err("fixture request headers too large".into());
        }
    }
}

#[tokio::test]
async fn upgrade_rejections_preserve_http_metadata_and_redact_debug() -> TestResult {
    let (listener, client) = peer().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let _ = request_headers(&mut stream).await?;
        let body = r#"{"error":{"code":"unauthorized_scope","message":"fixture-key","commit_status":"not_committed"}}"#;
        stream.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nWWW-Authenticate: Bearer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await?;
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let error = client
        .core_channel(ChannelOptions::default())
        .await
        .err()
        .ok_or("unauthorized upgrade succeeded")?;
    assert!(!format!("{error:?}").contains("fixture-key"));
    let Error::Core(failure) = error else {
        return Err("wrong HTTP error kind".into());
    };
    assert_eq!(failure.status, 401);
    assert_eq!(failure.headers["www-authenticate"], "Bearer");
    assert!(failure.body.is_some());
    assert!(failure.body_error.is_none());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn upgrade_redirects_never_forward_credentials() -> TestResult {
    let (listener, client) = peer().await?;
    let other = TcpListener::bind("127.0.0.1:0").await?;
    let location = format!("http://{}/steal", other.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let _ = request_headers(&mut stream).await?;
        stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?;
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    });
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.core_channel(ChannelOptions::default()),
    )
    .await?;
    assert!(matches!(result, Err(Error::Core(ref failure)) if failure.status == 302));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), other.accept())
            .await
            .is_err()
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn unsolicited_protocols_extensions_and_invalid_accept_headers_are_rejected() -> TestResult {
    for extra in [
        "Sec-WebSocket-Protocol: unexpected\r\n",
        "Sec-WebSocket-Extensions: permessage-deflate\r\n",
        "Sec-WebSocket-Accept: duplicate\r\n",
    ] {
        let (listener, client) = peer().await?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let headers = request_headers(&mut stream).await?;
            let key = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, value)| value.trim())
                .ok_or("missing key")?;
            let accept = derive_accept_key(key.as_bytes());
            stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extra}\r\n").as_bytes()).await?;
            Ok::<_, Box<dyn StdError + Send + Sync>>(())
        });
        let error = client
            .core_channel(ChannelOptions::default())
            .await
            .err()
            .ok_or("invalid upgrade succeeded")?;
        let Error::Core(failure) = error else {
            return Err("wrong handshake error kind".into());
        };
        assert_eq!(failure.status, 101);
        assert!(failure.body_error.is_some());
        server.await??;
    }
    Ok(())
}
