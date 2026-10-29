use super::*;

#[tokio::test]
async fn flattened_stream_is_rejected_before_all_six_generation_calls() -> TestResult {
    let server = serve(vec![]).await?;
    let client = GatewayClient::new(&server.url, None)?;
    let options = SubmissionOptions::default();
    for stream in [false, true] {
        let mut chat = chat_request(stream)?;
        let mut messages = messages_request(stream)?;
        let mut responses = responses_request(stream)?;
        for extra in [&mut chat.extra, &mut messages.extra, &mut responses.extra] {
            extra.insert("stream".into(), json!(!stream));
        }
        let errors = if stream {
            [
                client
                    .submit_stream_chat_completions(&chat, &options)
                    .await
                    .err(),
                client
                    .submit_stream_messages(&messages, &options)
                    .await
                    .err(),
                client
                    .submit_stream_responses(&responses, &options)
                    .await
                    .err(),
            ]
        } else {
            [
                client.submit_chat_completions(&chat, &options).await.err(),
                client.submit_messages(&messages, &options).await.err(),
                client.submit_responses(&responses, &options).await.err(),
            ]
        };
        assert_eq!(server.records.lock().await.len(), 0);
        assert!(
            errors
                .into_iter()
                .all(|error| matches!(error, Some(Error::Request(_))))
        );
    }
    Ok(())
}

fn protocol_failure(error: Option<Error>, code: u16) -> TestResult {
    let Some(Error::Service(failure)) = error else {
        return Err("malformed response accepted or evidence discarded".into());
    };
    assert_eq!(failure.status, code);
    assert_eq!(failure.headers["x-bitrouter-execution-id"], "execution-1");
    assert!(matches!(failure.body_error, Some(Error::Response(_))));
    Ok(())
}

#[tokio::test]
async fn existing_original_correlation_is_validated_independently() -> TestResult {
    for original in [
        String::new(),
        "c".repeat(257),
        "old\tcorrelation".into(),
        "old\ncorrelation".into(),
        "雪".into(),
        " original".into(),
        "original ".into(),
    ] {
        let mut body = status("execution-1", "generation");
        body["correlation_id"] = json!(original);
        let replies = (0..2)
            .map(|_| {
                let mut reply = shared(Reply::json(body.clone()), "execution-1", "current");
                reply.status = 202;
                reply
            })
            .collect();
        let server = serve(replies).await?;
        let client = GatewayClient::new(&server.url, None)?;
        let options = SubmissionOptions::default();
        protocol_failure(
            client
                .submit_chat_completions(&chat_request(false)?, &options)
                .await
                .err(),
            202,
        )?;
        protocol_failure(
            client
                .submit_stream_chat_completions(&chat_request(true)?, &options)
                .await
                .err(),
            202,
        )?;
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn existing_original_correlation_accepts_the_bound_without_matching_current() -> TestResult {
    let original = "c".repeat(256);
    let mut body = status("execution-1", "generation");
    body["correlation_id"] = json!(original);
    let mut reply = shared(Reply::json(body), "execution-1", "current");
    reply.status = 202;
    let server = serve(vec![reply]).await?;
    let result = GatewayClient::new(&server.url, None)?
        .submit_stream_chat_completions(&chat_request(true)?, &SubmissionOptions::default())
        .await?;
    let Submission::Existing {
        execution,
        metadata,
    } = result
    else {
        return Err("expected existing execution".into());
    };
    assert_eq!(execution.correlation_id, original);
    assert_eq!(metadata.correlation_id, "current");
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}

fn media_type(mut reply: Reply, value: &str) -> Reply {
    reply.headers.retain(|(name, _)| *name != "content-type");
    reply.headers.push(("content-type", value.into()));
    reply
}

#[tokio::test]
async fn combined_content_types_fail_for_json_existing_sse_and_query() -> TestResult {
    let json_type = "application/json; charset=utf-8, text/event-stream";
    let server = serve(vec![
        media_type(
            shared(Reply::json(chat_response()), "execution-1", "current"),
            json_type,
        ),
        media_type(duplicate("generation"), json_type),
        media_type(
            shared(Reply::sse("data: [DONE]\n\n"), "execution-1", "current"),
            "text/event-stream; charset=utf-8, application/json",
        ),
        media_type(
            shared(
                Reply::json(status("execution-1", "generation")),
                "execution-1",
                "original",
            ),
            json_type,
        ),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let options = SubmissionOptions::default();
    for code in [200, 202] {
        protocol_failure(
            client
                .submit_chat_completions(&chat_request(false)?, &options)
                .await
                .err(),
            code,
        )?;
    }
    protocol_failure(
        client
            .submit_stream_chat_completions(&chat_request(true)?, &options)
            .await
            .err(),
        200,
    )?;
    protocol_failure(client.execution("execution-1").await.err(), 200)?;
    assert_eq!(server.records.lock().await.len(), 4);
    Ok(())
}

#[tokio::test]
async fn complete_media_types_allow_case_charset_and_quoted_parameters() -> TestResult {
    let json_type =
        r#"Application/JSON; charset=UTF-8; note="comma, semi; escaped\"quote"; empty="""#;
    let server = serve(vec![
        media_type(
            shared(Reply::json(chat_response()), "execution-1", "current"),
            json_type,
        ),
        media_type(duplicate("generation"), json_type),
        media_type(
            shared(Reply::sse("data: [DONE]\n\n"), "execution-1", "current"),
            r#"Text/Event-Stream; charset=utf-8; note="a,b""#,
        ),
        media_type(
            shared(
                Reply::json(status("execution-1", "generation")),
                "execution-1",
                "original",
            ),
            json_type,
        ),
    ])
    .await?;
    let client = GatewayClient::new(&server.url, None)?;
    let options = SubmissionOptions::default();
    submitted(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    existing(
        client
            .submit_chat_completions(&chat_request(false)?, &options)
            .await?,
    )?;
    let (stream, _) = submitted(
        client
            .submit_stream_chat_completions(&chat_request(true)?, &options)
            .await?,
    )?;
    assert_eq!(drain(stream).await?, 0);
    assert_eq!(
        client.execution("execution-1").await?.correlation_id,
        "original"
    );
    assert_eq!(server.records.lock().await.len(), 4);
    Ok(())
}
