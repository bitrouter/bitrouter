use super::*;
use bitrouter_sdk::gateway::tenancy::{ErrorSource, Metadata};

fn with_metadata(mut reply: Response) -> Response {
    reply
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("12"));
    reply.headers_mut().insert(
        "x-bitrouter-error-source",
        HeaderValue::from_static("upstream"),
    );
    reply
        .headers_mut()
        .insert("set-cookie", HeaderValue::from_static("private-cookie"));
    reply
}

fn expected(status: u16) -> TestResult<Metadata> {
    Ok(Metadata {
        status,
        execution_id: Some("execution-1".try_into()?),
        retry_after_seconds: Some(12),
        error_source: Some(ErrorSource::Upstream),
    })
}

fn rate_error(upstream: bool) -> Value {
    json!({"error": {
        "message":"provider-private-detail", "type":"rate_limit_error",
        "code": if upstream { "upstream_rate_limited" } else { "rate_limit_exceeded" }
    }})
}

#[tokio::test]
async fn optional_metadata_preserves_absence_zero_and_numeric_bounds() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![Some(json_reply(200, Some("execution-1"), chat_json())?)];
        for seconds in ["0", "00012", "18446744073709551615"] {
            let mut reply = json_reply(200, Some("execution-1"), chat_json())?;
            reply
                .headers_mut()
                .insert("retry-after", HeaderValue::from_str(seconds)?);
            replies.push(Some(reply));
        }
        replies.push(Some(json_reply(429, None, rate_error(true))?));
        replies.push(Some(json_reply(
            409,
            None,
            tenancy_error(None, "not_started"),
        )?));
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("metadata")?;
        for seconds in [None, Some(0), Some(12), Some(u64::MAX)] {
            let reply = client
                .tenant("token", &bound)?
                .chat_reply(&chat(false)?)
                .await?;
            assert_eq!(reply.metadata.retry_after_seconds, seconds);
            assert_eq!(reply.metadata.error_source, None);
            assert_eq!(
                reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution-1")
            );
        }
        for state in [DispatchState::Unknown, DispatchState::NotStarted] {
            let error = client
                .tenant("token", &bound)?
                .chat_reply(&chat(false)?)
                .await
                .err()
                .ok_or("expected error")?;
            assert_eq!(error.dispatch_state(), state);
            let failed = failure::<()>(Err(error))?;
            assert_eq!(failed.problem, None);
            assert_eq!(failed.metadata.retry_after_seconds, None);
            assert_eq!(failed.metadata.error_source, None);
            assert_eq!(failed.metadata.execution_id, None);
        }
        assert_eq!(server.records.lock().await.len(), 6);
    }
    Ok(())
}

#[tokio::test]
async fn local_and_upstream_429_retain_distinct_transport_metadata() -> TestResult {
    for (version, wire) in versions() {
        let mut local = json_reply(429, Some("execution-1"), rate_error(false))?;
        local
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("61"));
        let server = server::serve(
            wire,
            vec![
                Some(local),
                Some(with_metadata(json_reply(
                    429,
                    Some("execution-1"),
                    rate_error(true),
                )?)),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let bound = binding("metadata")?;
        for upstream in [false, true] {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, None);
            assert_eq!(failed.metadata.status, 429);
            assert_eq!(
                failed.metadata.error_source,
                upstream.then_some(ErrorSource::Upstream)
            );
            assert_eq!(
                failed.metadata.retry_after_seconds,
                Some(if upstream { 12 } else { 61 })
            );
            assert_eq!(
                serde_json::to_value(failed.gateway_error.as_ref().ok_or("gateway envelope")?)?,
                rate_error(upstream)
            );
            assert!(!format!("{failed:?}").contains("provider-private-detail"));
            assert!(!format!("{failed:?}").contains("private-cookie"));
            assert_eq!(
                Error::Response(Box::new(failed)).dispatch_state(),
                DispatchState::Unknown
            );
        }
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_metadata_fails_closed_but_preserves_other_valid_fields() -> TestResult {
    for (version, wire) in versions() {
        let mut cases = Vec::new();
        for status in [200, 429] {
            for (name, values) in [
                ("retry-after", vec![""]),
                ("retry-after", vec!["-1"]),
                ("retry-after", vec!["+1"]),
                ("retry-after", vec!["1.5"]),
                ("retry-after", vec!["1 2"]),
                ("retry-after", vec!["12, 12"]),
                ("retry-after", vec!["18446744073709551616"]),
                ("retry-after", vec!["000000000000000000000"]),
                ("retry-after", vec!["Wed, 21 Oct 2015 07:28:00 GMT"]),
                ("retry-after", vec!["12", "12"]),
                ("retry-after", vec!["12", "13"]),
                ("x-bitrouter-error-source", vec![""]),
                ("x-bitrouter-error-source", vec!["local"]),
                ("x-bitrouter-error-source", vec!["Upstream"]),
                ("x-bitrouter-error-source", vec!["upstream, upstream"]),
                ("x-bitrouter-error-source", vec!["upstream", "upstream"]),
                ("x-bitrouter-error-source", vec!["upstream", "local"]),
            ] {
                let body = if status == 200 {
                    chat_json()
                } else {
                    rate_error(true)
                };
                let mut reply = with_metadata(json_reply(status, Some("execution-1"), body)?);
                reply.headers_mut().remove(name);
                for value in values {
                    reply
                        .headers_mut()
                        .append(name, HeaderValue::from_str(value)?);
                }
                cases.push((reply, status, name));
            }
            for name in ["retry-after", "x-bitrouter-error-source"] {
                let mut reply =
                    with_metadata(json_reply(status, Some("execution-1"), rate_error(true))?);
                reply
                    .headers_mut()
                    .insert(name, HeaderValue::from_bytes(b"\xff")?);
                cases.push((reply, status, name));
            }
        }
        let count = cases.len();
        let (replies, checks): (Vec<_>, Vec<_>) =
            cases.into_iter().map(|(r, s, n)| (Some(r), (s, n))).unzip();
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let bound = binding("metadata")?;
        for (status, name) in checks {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, Some(Problem::Metadata), "{name}");
            let mut metadata = expected(status)?;
            if name == "retry-after" {
                metadata.retry_after_seconds = None;
            } else {
                metadata.error_source = None;
            }
            assert_eq!(failed.metadata, metadata);
            if status == 429 {
                assert!(failed.gateway_error.is_some());
            }
            assert_eq!(
                Error::Response(Box::new(failed)).dispatch_state(),
                DispatchState::Unknown
            );
        }
        assert_eq!(server.records.lock().await.len(), count);
    }
    Ok(())
}

#[tokio::test]
async fn metadata_survives_json_sse_body_failures_and_identity_conflicts() -> TestResult {
    for (version, wire) in versions() {
        let malformed = Response::builder()
            .header("content-type", "application/json")
            .header("x-bitrouter-request-id", "execution-1")
            .body(HttpBody::from("{"))?;
        let server = server::serve(
            wire,
            vec![
                Some(with_metadata(json_reply(
                    200,
                    Some("execution-1"),
                    chat_json(),
                )?)),
                Some(with_metadata(sse("data: [DONE]\n\n")?)),
                Some(with_metadata(malformed)),
                Some(with_metadata(sse(
                    "event: error\ndata: {\"error\":{\"message\":\"private-stream-detail\"}}\n\n",
                )?)),
                Some(with_metadata(sse("")?)),
                Some(with_metadata(json_reply(
                    409,
                    Some("execution-1"),
                    tenancy_error(Some("execution-2"), "not_started"),
                )?)),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let bound = binding("metadata")?;
        let reply = client
            .tenant("token", &bound)?
            .chat_reply(&chat(false)?)
            .await?;
        assert_eq!(reply.metadata, expected(200)?);
        assert!(matches!(reply.body, Body::Json(_)));
        let reply = client
            .tenant("token", &bound)?
            .chat_reply(&chat(true)?)
            .await?;
        assert_eq!(reply.metadata, expected(200)?);
        let Body::Stream(stream) = reply.body else {
            return Err("expected stream".into());
        };
        assert_eq!(ends(stream).await?, ProtocolEnd::ChatDone);
        let failed = failure(
            client
                .tenant("token", &bound)?
                .chat_reply(&chat(false)?)
                .await,
        )?;
        assert_eq!(failed.metadata, expected(200)?);
        assert_eq!(failed.problem, Some(Problem::Contract));
        for problem in [None, Some(Problem::IncompleteStream)] {
            let reply = client
                .tenant("token", &bound)?
                .chat_reply(&chat(true)?)
                .await?;
            assert_eq!(reply.metadata, expected(200)?);
            let Body::Stream(stream) = reply.body else {
                return Err("expected stream".into());
            };
            let failed = stream_failure(stream).await?;
            assert_eq!(failed.metadata, expected(200)?);
            assert_eq!(failed.problem, problem);
            assert!(!format!("{failed:?}").contains("private-stream-detail"));
        }
        let failed = failure(
            client
                .tenant("token", &bound)?
                .chat_reply(&chat(false)?)
                .await,
        )?;
        assert_eq!(failed.metadata, expected(409)?);
        assert_eq!(failed.problem, Some(Problem::Metadata));
        assert!(failed.tenancy_error.is_some());
        assert_eq!(
            Error::Response(Box::new(failed)).dispatch_state(),
            DispatchState::Unknown
        );
        assert_eq!(server.records.lock().await.len(), 6);
    }
    Ok(())
}

#[tokio::test]
async fn error_body_failures_retain_valid_header_metadata() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = Vec::new();
        for body in ["{".to_owned(), "x".repeat(64)] {
            replies.push(Some(with_metadata(
                Response::builder()
                    .status(429)
                    .header("content-type", "application/json")
                    .header("x-bitrouter-request-id", "execution-1")
                    .body(HttpBody::from(body))?,
            )));
        }
        let server = server::serve(wire, replies).await?;
        let client = GatewayClient::with_options(
            &server.endpoint,
            None,
            TransportOptions {
                http_version: version,
                max_response_bytes: 32,
                request_timeout: Some(BUDGET),
                ..Default::default()
            },
        )?;
        let bound = binding("metadata")?;
        for problem in [Problem::Contract, Problem::Limit] {
            let failed = failure(
                client
                    .tenant("token", &bound)?
                    .chat_reply(&chat(false)?)
                    .await,
            )?;
            assert_eq!(failed.problem, Some(problem));
            assert_eq!(failed.metadata, expected(429)?);
            assert!(failed.gateway_error.is_none());
            assert_eq!(
                Error::Response(Box::new(failed)).dispatch_state(),
                DispatchState::Unknown
            );
        }
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}
