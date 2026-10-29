use super::*;
use reqwest::header::HeaderValue;

#[tokio::test]
async fn identity_normalization_semantic_duplicates_and_stable_user_agent() -> TestResult {
    let correlation = "c".repeat(256);
    let server = serve(vec![
        shared(Reply::json(chat_response()), "execution-1", &correlation),
        shared(Reply::json(chat_response()), "execution-1", &correlation),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, Some("configured-credential"))?;
    let mut options = SubmissionOptions {
        correlation_id: Some(format!(" \t{correlation}\t ")),
        idempotency_key: Some(" \tkey\t ".into()),
        ..Default::default()
    };
    options
        .headers
        .append("openai-project", HeaderValue::from_static("p1"));
    options
        .headers
        .append("openai-project", HeaderValue::from_static("p2"));
    options
        .headers
        .insert("session-id", HeaderValue::from_static(""));
    submitted(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    options
        .headers
        .append("user-agent", HeaderValue::from_static("custom-a"));
    options
        .headers
        .append("user-agent", HeaderValue::from_static("custom-b"));
    submitted(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    for record in records.iter() {
        assert_eq!(record.headers["x-bitrouter-request-id"], correlation);
        assert_eq!(record.headers["idempotency-key"], "key");
        assert_eq!(
            record
                .headers
                .get_all("openai-project")
                .iter()
                .map(|v| v.as_bytes())
                .collect::<Vec<_>>(),
            vec![b"p1".as_slice(), b"p2".as_slice()]
        );
        assert_eq!(record.headers["session-id"], "");
        assert_eq!(
            record.credential.as_deref(),
            Some("Bearer configured-credential")
        );
        assert_eq!(record.headers["content-type"], "application/json");
    }
    assert_eq!(records[0].headers.get_all("user-agent").iter().count(), 1);
    assert_eq!(records[0].headers["user-agent"], "bitrouter-sdk");
    assert_eq!(
        records[1]
            .headers
            .get_all("user-agent")
            .iter()
            .map(|v| v.as_bytes())
            .collect::<Vec<_>>(),
        vec![b"custom-a".as_slice(), b"custom-b".as_slice()]
    );
    Ok(())
}

#[tokio::test]
async fn invalid_options_profiles_and_stream_modes_fail_before_network() -> TestResult {
    let server = serve(vec![]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    for name in [
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "content-type",
        "accept",
        "host",
        "cookie",
        "connection",
        "content-length",
        "transfer-encoding",
        "bitrouter-beta",
        "x-bitrouter-inbound-protocol",
        "x-bitrouter-request-id",
        "idempotency-key",
        "x-bitrouter-execution-id",
        "x-tenant-id",
        "x-unknown",
    ] {
        let mut options = SubmissionOptions::default();
        options
            .headers
            .insert(name, HeaderValue::from_static("private-value"));
        assert!(matches!(
            client
                .submit_chat_completions(&chat_request(false)?, &options)
                .await,
            Err(Error::Request(_))
        ));
    }
    for value in [
        "".into(),
        " ".into(),
        "c".repeat(257),
        "x\ty".into(),
        "x\r\ny".into(),
        "雪".into(),
    ] {
        for correlation in [true, false] {
            let mut options = SubmissionOptions::default();
            if correlation {
                options.correlation_id = Some(value.clone());
            } else {
                options.idempotency_key = Some(value.clone());
            }
            assert!(matches!(
                client
                    .submit_chat_completions(&chat_request(false)?, &options)
                    .await,
                Err(Error::Request(_))
            ));
        }
    }
    for (field, value) in [
        ("bitrouter", Value::Null),
        ("bitrouter", json!({})),
        ("multi_agent", json!({"enabled":true})),
    ] {
        for stream in [true, false] {
            let mut request = responses_request(stream)?;
            request.extra.insert(field.into(), value.clone());
            if stream {
                assert!(matches!(
                    client
                        .submit_stream_responses(&request, &SubmissionOptions::default())
                        .await,
                    Err(Error::Request(_))
                ));
            } else {
                assert!(matches!(
                    client
                        .submit_responses(&request, &SubmissionOptions::default())
                        .await,
                    Err(Error::Request(_))
                ));
            }
        }
    }
    assert!(matches!(
        client
            .submit_chat_completions(&chat_request(true)?, &SubmissionOptions::default())
            .await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        client
            .submit_stream_chat_completions(&chat_request(false)?, &SubmissionOptions::default())
            .await,
        Err(Error::Request(_))
    ));
    for id in [
        "".into(),
        ".".into(),
        "..".into(),
        "\n".into(),
        "x".repeat(257),
    ] {
        assert!(matches!(
            client.execution(&id).await,
            Err(Error::Request(_))
        ));
    }
    assert_eq!(server.records.lock().await.len(), 0);
    Ok(())
}

#[tokio::test]
async fn ordinary_responses_extensions_and_legacy_user_agent_remain_available() -> TestResult {
    let output =
        json!({"id":"r","object":"response","model":"route","status":"completed","output":[]});
    let server = serve(vec![
        shared(Reply::json(output.clone()), "execution-1", "current"),
        Reply::json(output),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let mut request = responses_request(false)?;
    request
        .extra
        .insert("multi_agent".into(), json!({"enabled":false}));
    request
        .extra
        .insert("extension".into(), json!({"marker":"preserved"}));
    submitted(
        client
            .submit_responses(&request, &SubmissionOptions::default())
            .await?,
    )?;
    client.responses(&request).await?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    for record in records.iter() {
        let body: Value = serde_json::from_slice(&record.body)?;
        assert_eq!(body["multi_agent"]["enabled"], false);
        assert_eq!(body["extension"]["marker"], "preserved");
        assert!(!record.headers.contains_key("idempotency-key"));
    }
    assert_eq!(records[0].headers["user-agent"], "bitrouter-sdk");
    assert_eq!(
        records[1].headers["user-agent"],
        concat!("bitrouter-sdk/", env!("CARGO_PKG_VERSION"))
    );
    Ok(())
}

#[test]
fn new_debug_boundaries_do_not_require_or_expose_payload_debug() -> TestResult {
    struct NoDebug;
    let metadata = ExecutionMetadata {
        execution_id: "private-execution".into(),
        correlation_id: "private-correlation".into(),
        status_location: "https://private-location".into(),
    };
    let options = SubmissionOptions {
        correlation_id: Some("private-correlation".into()),
        idempotency_key: Some("private-binding".into()),
        ..Default::default()
    };
    let first = Submission::Response {
        response: NoDebug,
        metadata: metadata.clone(),
    };
    let existing: Submission<NoDebug> = Submission::Existing {
        execution: Box::new(serde_json::from_value(status(
            "private-execution",
            "generation",
        ))?),
        metadata: metadata.clone(),
    };
    for value in [
        format!("{options:?}"),
        format!("{metadata:?}"),
        format!("{first:?}"),
        format!("{existing:?}"),
    ] {
        assert!(!value.contains("private-"));
    }
    Ok(())
}
