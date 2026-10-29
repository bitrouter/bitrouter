use super::*;

fn retained(error: Error, expected: u16) -> TestResult<Box<bitrouter_sdk::error::ServiceFailure>> {
    let Error::Service(failure) = error else {
        return Err("response evidence was discarded".into());
    };
    assert_eq!(failure.status, expected);
    assert!(failure.body_error.is_some());
    Ok(failure)
}

#[tokio::test]
async fn legacy_model_methods_reject_202_without_replaying_or_decoding_it() -> TestResult {
    let server = serve(vec![
        duplicate("generation"),
        duplicate("generation"),
        duplicate("generation"),
        duplicate("decisions"),
        duplicate("generation"),
        duplicate("generation"),
        duplicate("generation"),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let errors = [
        client.chat_completions(&chat_request(false)?).await.err(),
        client.messages(&messages_request(false)?).await.err(),
        client.responses(&responses_request(false)?).await.err(),
        client.decisions(&decisions_request()?).await.err(),
        client
            .stream_chat_completions(&chat_request(true)?)
            .await
            .err(),
        client.stream_messages(&messages_request(true)?).await.err(),
        client
            .stream_responses(&responses_request(true)?)
            .await
            .err(),
    ];
    for error in errors {
        let failure = retained(error.ok_or("legacy method accepted 202")?, 202)?;
        assert_eq!(failure.headers["x-bitrouter-execution-id"], "execution-1");
    }
    assert_eq!(server.records.lock().await.len(), 7);
    Ok(())
}

#[tokio::test]
async fn malformed_success_and_limits_preserve_execution_evidence() -> TestResult {
    for code in [200, 202] {
        for mode in [
            "invalid-json",
            "oversized",
            "wrong-type",
            "duplicate-type",
            "wrong-contract",
        ] {
            let mut reply = shared(Reply::json(json!({"wrong":true})), "execution-1", "current");
            reply.status = code;
            match mode {
                "invalid-json" => reply.chunks = vec![b"private-not-json".to_vec()],
                "oversized" => reply.chunks = vec![vec![b' '; 4097]],
                "wrong-type" => reply.headers[0].1 = "text/plain".into(),
                "duplicate-type" => reply
                    .headers
                    .push(("content-type", "application/json".into())),
                _ => {}
            }
            let server = serve(vec![reply]).await?;
            let client = GatewayClient::with_options(
                &server.url,
                None,
                TransportOptions {
                    max_response_bytes: 4096,
                    ..Default::default()
                },
            )?;
            let error = client
                .submit_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
                .await
                .err()
                .ok_or("malformed success accepted")?;
            assert!(!format!("{error:?}").contains("private-not-json"));
            let failure = retained(error, code)?;
            assert_eq!(failure.headers["x-bitrouter-execution-id"], "execution-1");
            if mode == "oversized" {
                assert!(matches!(failure.body_error, Some(Error::Limit)));
            }
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn metadata_is_required_unambiguous_and_bound_to_the_response() -> TestResult {
    for field in [
        "x-bitrouter-execution-id",
        "x-bitrouter-request-id",
        "location",
    ] {
        for mode in ["missing", "duplicate", "invalid"] {
            let mut reply = duplicate("generation");
            match mode {
                "missing" => reply.headers.retain(|(name, _)| *name != field),
                "duplicate" => reply.headers.push((field, "duplicate".into())),
                _ => {
                    for (name, value) in &mut reply.headers {
                        if *name == field {
                            *value = if field == "location" {
                                "https://other.invalid/v1/gateway/executions/execution-1".into()
                            } else {
                                "wrong-identity".into()
                            };
                        }
                    }
                }
            }
            let server = serve(vec![reply]).await?;
            let client = GatewayClient::new(&server.url, None)?;
            let options = SubmissionOptions {
                correlation_id: Some("current".into()),
                ..Default::default()
            };
            let error = client
                .submit_chat_completions(&chat_request(false)?, &options)
                .await
                .err()
                .ok_or("invalid identity accepted")?;
            retained(error, 202)?;
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    // Operation and query identity are checked independently of header validity.
    let server = serve(vec![
        duplicate("decisions"),
        shared(
            Reply::json(status("other-id", "generation")),
            "execution-1",
            "original",
        ),
        shared(
            Reply::json(status("execution-1", "generation")),
            "execution-1",
            "wrong-correlation",
        ),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    retained(
        client
            .submit_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
            .await
            .err()
            .ok_or("wrong operation accepted")?,
        202,
    )?;
    for _ in 0..2 {
        retained(
            client
                .execution("execution-1")
                .await
                .err()
                .ok_or("wrong query identity accepted")?,
            200,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn local_success_and_unexpected_2xx_fail_without_fallback() -> TestResult {
    for code in [200, 201, 204, 206] {
        let mut reply = Reply::json(chat_response());
        reply.status = code;
        let server = serve(vec![reply]).await?;
        let client = GatewayClient::new(&server.url, None)?;
        retained(
            client
                .submit_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
                .await
                .err()
                .ok_or("non-shared success accepted")?,
            code,
        )?;
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn candidate_and_service_failures_preserve_status_without_success_validation() -> TestResult {
    for code in [401, 403, 404, 409, 429, 503] {
        let mut reply = shared(
            Reply::json(json!({"error":{"message":"private-failure","type":"fixture"}})),
            "candidate",
            "current",
        );
        reply.status = code;
        reply.headers.extend([
            ("retry-after", "3".into()),
            ("www-authenticate", "Bearer".into()),
            ("location", "duplicate-invalid-location".into()),
        ]);
        let server = serve(vec![reply]).await?;
        let client = GatewayClient::new(&server.url, None)?;
        let Error::Service(failure) = client
            .submit_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
            .await
            .err()
            .ok_or("service failure accepted")?
        else {
            return Err("wrong service failure".into());
        };
        assert_eq!(failure.status, code);
        assert_eq!(failure.headers["x-bitrouter-execution-id"], "candidate");
        assert_eq!(failure.headers["retry-after"], "3");
        assert!(failure.body.is_some());
        assert!(failure.body_error.is_none());
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn truncated_200_and_202_preserve_identity_after_headers_arrive() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for code in [200, 202] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut input = [0; 4096];
            let _ = socket.read(&mut input).await?;
            // Deliberately violate the declared HTTP/1.1 body length after
            // complete headers: https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3
            let head = format!(
                "HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\n\
                 X-Bitrouter-Execution-Id: execution-1\r\nX-Bitrouter-Request-Id: current\r\n\
                 Location: /v1/gateway/executions/execution-1\r\n\
                 Content-Length: 1000\r\nConnection: close\r\n\r\n{{\"partial"
            );
            socket.write_all(head.as_bytes()).await?;
            socket.shutdown().await
        });
        let error = GatewayClient::new(&endpoint, None)?
            .submit_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
            .await
            .err()
            .ok_or("truncated response accepted")?;
        servers.join_next().await.ok_or("missing server task")???;
        let failure = retained(error, code)?;
        assert_eq!(failure.headers["x-bitrouter-execution-id"], "execution-1");
        assert!(matches!(failure.body_error, Some(Error::Transport(_))));
    }
    Ok(())
}
