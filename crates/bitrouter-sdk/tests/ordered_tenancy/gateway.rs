use super::*;
use bitrouter_ai::{
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
    stream::{ProtocolEnd, StreamItem},
    tenancy::operation::{DispatchBinding, DispatchState},
};
use bitrouter_sdk::gateway::tenancy::{Body, Error, ErrorSource, EventStream, Problem};
use futures_util::StreamExt;

#[path = "gateway/defaults.rs"]
mod defaults;
#[path = "gateway/envelope.rs"]
mod envelope;
#[path = "gateway/envelope_v2.rs"]
mod envelope_v2;
#[path = "gateway/native_controls.rs"]
mod native_controls;

fn request(protocol: usize, stream: bool) -> Value {
    match protocol {
        0 => json!({"model":"route","messages":[],"stream":stream}),
        1 => json!({"model":"route","messages":[],"max_tokens":20,"stream":stream}),
        _ => json!({"model":"route","input":"hello","stream":stream}),
    }
}
fn serialized_request(protocol: usize, stream: bool) -> TestResult<Value> {
    let value = request(protocol, stream);
    Ok(match protocol {
        0 => serde_json::to_value(serde_json::from_value::<ChatRequest>(value)?)?,
        1 => serde_json::to_value(serde_json::from_value::<MessagesRequest>(value)?)?,
        _ => serde_json::to_value(serde_json::from_value::<ResponsesRequest>(value)?)?,
    })
}
fn output(protocol: usize) -> Value {
    match protocol {
        0 => json!({"id":"provider-id","object":"chat.completion","model":"route","choices":[]}),
        1 => {
            json!({"id":"provider-id","type":"message","role":"assistant","model":"route","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
        }
        _ => {
            json!({"id":"provider-id","object":"response","model":"route","status":"completed","output":[]})
        }
    }
}
fn reply(protocol: usize, stream: bool) -> TestResult<Response> {
    let body = if stream {
        match protocol {
            0 => "data: [DONE]\n\n".into(),
            1 => "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into(),
            _ => format!(
                "event: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{}}}\n\n",
                output(protocol)
            ),
        }
    } else {
        output(protocol).to_string()
    };
    Ok(Response::builder()
        .header(
            "content-type",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header("x-bitrouter-request-id", "execution")
        .header("retry-after", "12")
        .header("x-bitrouter-error-source", "upstream")
        .body(HttpBody::from(body))?)
}
async fn end<T>(mut stream: EventStream<T>) -> TestResult<ProtocolEnd> {
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        if let StreamItem::End(value) = item? {
            assert!(terminal.is_none());
            terminal = Some(value);
        }
    }
    terminal.ok_or_else(|| "missing terminal".into())
}
async fn dispatch(
    client: &GatewayClient,
    binding: &OrderedDispatchBindingV2,
    protocol: usize,
    stream: bool,
) -> TestResult {
    let scoped = client.ordered_tenant("scoped-token", binding)?;
    assert!(!format!("{scoped:?}").contains("scoped-token"));
    macro_rules! call {
        ($method:ident, $request:ty, $terminal:expr) => {{
            let input: $request = serde_json::from_value(request(protocol, stream))?;
            let result = scoped.$method(&input).await?;
            assert_eq!(result.metadata.status, 200);
            assert_eq!(
                result.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
            assert_eq!(result.metadata.retry_after_seconds, Some(12));
            assert_eq!(result.metadata.error_source, Some(ErrorSource::Upstream));
            match result.body {
                Body::Json(value) => {
                    assert!(!stream);
                    assert_eq!(serde_json::to_value(value)?, output(protocol));
                }
                Body::Stream(events) => {
                    assert!(stream);
                    assert_eq!(end(events).await?, $terminal);
                }
            }
        }};
    }
    match protocol {
        0 => call!(chat_reply, ChatRequest, ProtocolEnd::ChatDone),
        1 => call!(messages_reply, MessagesRequest, ProtocolEnd::MessagesStop),
        _ => call!(
            responses_reply,
            ResponsesRequest,
            ProtocolEnd::ResponsesCompleted
        ),
    }
    Ok(())
}

// Independent fixture decoder; the production helper only encodes.
fn decode_selection(text: &str) -> TestResult<Value> {
    assert!(text.len() <= 4096 && !text.contains('='));
    let mut word = 0_u32;
    let mut bits = 0;
    let mut decoded = Vec::new();
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err("invalid base64url".into()),
        };
        word = (word << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((word >> bits) as u8);
        }
        word &= (1 << bits) - 1;
    }
    assert_eq!(word, 0, "noncanonical pad bits");
    Ok(serde_json::from_slice(&decoded)?)
}

#[tokio::test]
async fn three_protocols_reuse_pool_and_keep_exact_v2_headers_out_of_bodies() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![Some(server::json(json!({"status":"ok"}))?)];
        for protocol in 0..3 {
            for stream in [false, true] {
                replies.push(Some(reply(protocol, stream)?));
            }
        }
        replies.push(Some(server::json(json!({"status":"ok"}))?));
        let server = server::serve(wire, replies).await?;
        let client = gateway_client(&server, version)?;
        let bound = binding(vec![255, 8, 0])?;
        client.health().await?;
        for protocol in 0..3 {
            for stream in [false, true] {
                dispatch(&client, &bound, protocol, stream).await?;
            }
        }
        client.health().await?;
        let records = server.records.lock().await;
        assert_eq!(records.len(), 8);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.version, wire);
            assert_eq!(record.connection, records[0].connection);
            if index == 0 || index == 7 {
                assert_eq!(record.headers["authorization"], "Bearer default-token");
                assert!(
                    !record
                        .headers
                        .keys()
                        .any(|key| key.as_str().starts_with("bitrouter-"))
                );
                continue;
            }
            let protocol = (index - 1) / 2;
            let stream = (index - 1) % 2 == 1;
            assert_eq!(record.method, "POST");
            assert_eq!(
                record.path,
                [
                    "/prefix/v1/chat/completions",
                    "/prefix/v1/messages",
                    "/prefix/v1/responses"
                ][protocol]
            );
            assert_eq!(
                serde_json::from_slice::<Value>(&record.body)?,
                serialized_request(protocol, stream)?
            );
            assert_eq!(record.headers["authorization"], "Bearer scoped-token");
            for (name, value) in [
                ("bitrouter-tenancy-version", "2"),
                ("bitrouter-installation-id", "installation"),
                ("bitrouter-instance-id", "instance"),
                ("bitrouter-capability-revision", "capability"),
                ("bitrouter-execution-revision", "revision"),
                ("bitrouter-correlation-id", "correlation"),
                ("bitrouter-receipt-mode", "strict"),
            ] {
                assert_eq!(record.headers[name], value);
                assert_eq!(record.headers.get_all(name).iter().count(), 1);
            }
            assert_eq!(
                record
                    .headers
                    .keys()
                    .filter(|key| key.as_str().starts_with("bitrouter-"))
                    .count(),
                8
            );
            assert_eq!(
                record
                    .headers
                    .get_all("bitrouter-candidate-selection")
                    .iter()
                    .count(),
                1
            );
            assert_eq!(
                decode_selection(record.headers["bitrouter-candidate-selection"].to_str()?)?,
                serde_json::to_value(&bound.selection)?
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn accepted_counts_are_never_truncated_and_invalid_selection_never_sends() -> TestResult {
    for (version, wire) in versions() {
        let replies = (0..4)
            .map(|_| reply(0, false).map(Some))
            .collect::<TestResult<Vec<_>>>()?;
        let server = server::serve(wire, replies).await?;
        let client = gateway_client(&server, version)?;
        for count in [1, 8, 9, 64] {
            dispatch(&client, &binding((0..count).rev().collect())?, 0, false).await?;
        }
        for indices in [vec![], vec![0, 0], vec![256], (0..65).collect()] {
            let mut bound = binding(vec![0])?;
            bound.selection.candidate_indices = indices;
            assert!(matches!(
                client.ordered_tenant("token", &bound),
                Err(Error::Request(_))
            ));
        }
        assert!(
            capabilities(8)?
                .bind(
                    "revision".try_into()?,
                    "correlation".try_into()?,
                    selection((0..9).collect())?
                )
                .is_err()
        );
        let mut bound = binding(vec![0])?;
        for credential in ["", "Bearer token", "token\r\ninjected", "to=ken", "==="] {
            assert!(client.ordered_tenant(credential, &bound).is_err());
        }
        bound.contract_version = "1".into();
        assert!(client.ordered_tenant("token", &bound).is_err());
        bound.contract_version = "2".into();
        bound.selection.schema_version = 2;
        assert!(client.ordered_tenant("token", &bound).is_err());
        let mut input: ChatRequest = serde_json::from_value(request(0, false))?;
        input.model = "different-route".into();
        assert!(matches!(
            client
                .ordered_tenant("token", &binding(vec![0])?)?
                .chat_reply(&input)
                .await,
            Err(Error::Request(_))
        ));
        input.model = "route".into();
        for (key, value) in [("model", json!("route")), ("stream", json!(false))] {
            input.extra.insert(key.into(), value);
            assert!(matches!(
                client
                    .ordered_tenant("token", &binding(vec![0])?)?
                    .chat_reply(&input)
                    .await,
                Err(Error::Request(_))
            ));
            input.extra.clear();
        }
        let mut v1 = serde_json::to_value(binding(vec![0])?)?;
        v1["Bitrouter-Tenancy-Version"] = json!("1");
        assert!(
            serde_json::from_value::<DispatchBinding>(v1).is_err(),
            "v1 cannot accept selection"
        );
        let records = server.records.lock().await;
        assert_eq!(records.len(), 4);
        for (record, count) in records.iter().zip([1, 8, 9, 64]) {
            let selection =
                decode_selection(record.headers["bitrouter-candidate-selection"].to_str()?)?;
            assert_eq!(
                selection["candidate_indices"],
                json!((0..count).rev().collect::<Vec<_>>())
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_callers_keep_selections_and_credentials_request_local() -> TestResult {
    for (version, wire) in versions() {
        let server =
            server::serve(wire, vec![Some(reply(0, false)?), Some(reply(0, false)?)]).await?;
        let client = gateway_client(&server, version)?;
        let mut first = binding(vec![9, 0, 8])?;
        first.correlation_id = "first".try_into()?;
        let mut second = binding(vec![255, 2])?;
        second.correlation_id = "second".try_into()?;
        let input: ChatRequest = serde_json::from_value(request(0, false))?;
        let first_call = client.ordered_tenant("first-token", &first)?;
        let second_call = client.ordered_tenant("second-token", &second)?;
        let (left, right) = tokio::join!(
            first_call.chat_reply(&input),
            second_call.chat_reply(&input)
        );
        assert!(matches!(left?.body, Body::Json(_)));
        assert!(matches!(right?.body, Body::Json(_)));
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        for record in records.iter() {
            let (expected, credential) = if record.headers["bitrouter-correlation-id"] == "first" {
                (&first, "Bearer first-token")
            } else {
                assert_eq!(record.headers["bitrouter-correlation-id"], "second");
                (&second, "Bearer second-token")
            };
            assert_eq!(record.headers["authorization"], credential);
            assert_eq!(
                decode_selection(record.headers["bitrouter-candidate-selection"].to_str()?)?,
                serde_json::to_value(&expected.selection)?
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn redirects_never_forward_scoped_headers_or_submit_again() -> TestResult {
    for (version, wire) in versions() {
        let other = server::serve(wire, vec![Some(reply(0, false)?)]).await?;
        let redirected = Response::builder()
            .status(307)
            .header(
                "location",
                format!("{}/v1/chat/completions", other.endpoint),
            )
            .header("content-type", "application/json")
            .body(HttpBody::from(error_body().to_string()))?;
        let server = server::serve(
            wire,
            vec![
                Some(redirected),
                Some(server::json(json!({"status":"ok"}))?),
            ],
        )
        .await?;
        let client = gateway_client(&server, version)?;
        let input: ChatRequest = serde_json::from_value(request(0, false))?;
        let error = client
            .ordered_tenant("secret-token", &binding(vec![0])?)?
            .chat_reply(&input)
            .await
            .err()
            .ok_or("redirect accepted")?;
        assert_eq!(error.dispatch_state(), DispatchState::Unknown);
        assert!(!format!("{error:?}").contains("secret-token"));
        client.health().await?;
        assert_eq!(server.records.lock().await.len(), 2);
        assert!(other.records.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn old_server_rejection_and_lost_response_never_downgrade_or_retry() -> TestResult {
    for (version, wire) in versions() {
        let server = server::serve(
            wire,
            vec![
                Some(json_status(409, error_body())?),
                None,
                Some(server::json(json!({"status":"ok"}))?),
            ],
        )
        .await?;
        let client = gateway_client(&server, version)?;
        let input: ChatRequest = serde_json::from_value(request(0, false))?;
        let error = client
            .ordered_tenant("token", &binding(vec![0])?)?
            .chat_reply(&input)
            .await
            .err()
            .ok_or("expected rejection")?;
        assert_eq!(error.dispatch_state(), DispatchState::NotStarted);
        let error = client
            .ordered_tenant("token", &binding(vec![0])?)?
            .chat_reply(&input)
            .await
            .err()
            .ok_or("expected transport loss")?;
        assert!(matches!(error, Error::Transport));
        assert_eq!(error.dispatch_state(), DispatchState::Unknown);
        client.health().await?;
        let records = server.records.lock().await;
        assert_eq!(records.len(), 3);
        assert!(
            records[..2]
                .iter()
                .all(|record| record.headers["bitrouter-tenancy-version"] == "2")
        );
    }
    Ok(())
}

#[tokio::test]
async fn ordered_calls_share_metadata_and_body_failure_validation() -> TestResult {
    for (version, wire) in versions() {
        let mut duplicate = reply(0, false)?;
        duplicate.headers_mut().append("retry-after", "13".parse()?);
        let broken = Response::builder()
            .header("content-type", "application/json")
            .header("x-bitrouter-request-id", "execution")
            .header("retry-after", "12")
            .body(HttpBody::from("{"))?;
        let server = server::serve(wire, vec![Some(duplicate), Some(broken)]).await?;
        let client = gateway_client(&server, version)?;
        for problem in [Problem::Metadata, Problem::Contract] {
            let input: ChatRequest = serde_json::from_value(request(0, false))?;
            let error = client
                .ordered_tenant("token", &binding(vec![0])?)?
                .chat_reply(&input)
                .await
                .err()
                .ok_or("expected failure")?;
            assert_eq!(error.dispatch_state(), DispatchState::Unknown);
            let Error::Response(failure) = error else {
                return Err("response failure".into());
            };
            assert_eq!(failure.problem, Some(problem));
            assert_eq!(
                failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
        }
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}
