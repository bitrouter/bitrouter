//! Additional operator response and query boundary coverage.

use super::*;

#[tokio::test]
async fn definite_rejections_require_matching_status_code_and_identity() -> Result {
    for (status, code) in [
        (400, ErrorCode::InvalidInput),
        (401, ErrorCode::Unauthorized),
        (403, ErrorCode::Forbidden),
        (404, ErrorCode::NotFound),
        (409, ErrorCode::Conflict),
    ] {
        for operation_id in [None, Some("..")] {
            let server = serve(vec![Reply {
                status,
                body: json!({"code":code,"operation_id":operation_id}),
                location: None,
            }])
            .await?;
            let result = OperatorClient::new(&server.endpoint, TOKEN)?
                .apply(&input())
                .await;
            let count = server.requests.lock().await.len();
            server.finish().await?;
            assert!(matches!(result,Err(Error::OperatorRejected(actual)) if actual==code));
            assert_eq!(count, 1);
        }
    }
    for (status, body) in [
        (403, json!({"code":"forbidden","operation_id":"other"})),
        (403, json!({"code":"conflict","operation_id":".."})),
        (403, json!({"private":SECRET})),
        (503, json!({"code":"unavailable","operation_id":".."})),
        (503, json!({"code":"unknown","operation_id":".."})),
        (201, receipt()),
    ] {
        let server = serve(vec![Reply {
            status,
            body,
            location: None,
        }])
        .await?;
        let result = OperatorClient::new(&server.endpoint, TOKEN)?
            .apply(&input())
            .await;
        let count = server.requests.lock().await.len();
        server.finish().await?;
        assert!(
            matches!(result,Err(Error::OperatorUnknown {operation_id,..}) if operation_id=="..")
        );
        assert_eq!(count, 1);
    }
    Ok(())
}

#[tokio::test]
async fn operator_responses_require_single_complete_json_media_type_and_bounded_streams() -> Result
{
    let mut responses = Vec::new();
    for media in [
        "text/plain",
        "application/json; charset=",
        "application/json, text/plain",
    ] {
        responses.push(
            Response::builder()
                .header("content-type", media)
                .body(Body::from(receipt().to_string()))?,
        );
    }
    responses.push(
        Response::builder()
            .header("content-type", "application/json")
            .header("content-type", "application/json")
            .body(Body::from(receipt().to_string()))?,
    );
    responses.push(
        Response::builder()
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(Body::from(receipt().to_string()))?,
    );
    let truncated = Body::from_stream(futures_util::stream::iter([
        Ok(bytes::Bytes::from_static(b"{\"namespace\":")),
        Err(std::io::Error::other("fixture body interrupted")),
    ]));
    responses.push(
        Response::builder()
            .header("content-type", "application/json")
            .body(truncated)?,
    );
    for response in responses {
        let server = serve_responses(vec![response]).await?;
        let client = OperatorClient::with_options(
            &server.endpoint,
            TOKEN,
            OperatorOptions {
                max_response_bytes: 1024,
                ..OperatorOptions::default()
            },
        )?;
        let result = client.apply(&input()).await;
        let count = server.requests.lock().await.len();
        server.finish().await?;
        let error = result.err().ok_or("accepted invalid response")?;
        assert!(matches!(&error,Error::OperatorUnknown {operation_id,..} if operation_id==".."));
        assert!(!format!("{error:?}").contains(SECRET));
        assert_eq!(count, 1);
    }
    Ok(())
}

#[tokio::test]
async fn streamed_valid_receipt_exceeding_bound_reports_limit() -> Result {
    let mut bytes = receipt().to_string().into_bytes();
    bytes.resize(1536, b' ');
    let body = Body::from_stream(futures_util::stream::iter([
        Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&bytes[..768])),
        Ok(bytes::Bytes::copy_from_slice(&bytes[768..])),
    ]));
    let server = serve_responses(vec![
        Response::builder()
            .header("content-type", "application/json")
            .body(body)?,
    ])
    .await?;
    let client = OperatorClient::with_options(
        &server.endpoint,
        TOKEN,
        OperatorOptions {
            max_response_bytes: 1024,
            ..OperatorOptions::default()
        },
    )?;
    let result = client.apply(&input()).await;
    let count = server.requests.lock().await.len();
    server.finish().await?;
    assert!(
        matches!(result,Err(Error::OperatorUnknown {source,..}) if matches!(*source,Error::Limit))
    );
    assert_eq!(count, 1);
    Ok(())
}

fn compressed_response(status: u16, value: Value) -> Result<Response> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(value.to_string().as_bytes())?;
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .body(Body::from(encoder.finish()?))?)
}

#[tokio::test]
async fn operator_rejects_compression_even_with_unified_decoder_features() -> Result {
    // The feature-unification campaign enables reqwest/gzip and asks this control
    // client to prove decoding is active before testing the operator opt-out.
    if std::env::var_os("BITROUTER_TEST_EXPECT_GZIP").is_some() {
        let server = serve_responses(vec![compressed_response(200, receipt())?]).await?;
        let result = async {
            let response = reqwest::Client::new().get(&server.endpoint).send().await?;
            assert!(!response.headers().contains_key("content-encoding"));
            assert_eq!(response.json::<Value>().await?, receipt());
            Ok::<_, Box<dyn StdError + Send + Sync>>(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    for (status, body) in [
        (200, receipt()),
        (403, json!({"code":"forbidden","operation_id":".."})),
    ] {
        let server = serve_responses(vec![compressed_response(status, body)?]).await?;
        let result = OperatorClient::new(&server.endpoint, TOKEN)?
            .apply(&input())
            .await;
        let count = server.requests.lock().await.len();
        server.finish().await?;
        assert!(matches!(result, Err(Error::OperatorUnknown { .. })));
        assert_eq!(count, 1);
    }
    let server = serve_responses(vec![compressed_response(
        200,
        json!({"config_generation":1,"key_generation":1}),
    )?])
    .await?;
    let result = OperatorClient::new(&server.endpoint, TOKEN)?
        .revision()
        .await;
    server.finish().await?;
    assert!(matches!(result, Err(Error::Response(_))));
    Ok(())
}

fn metadata(case: usize, id: &str) -> Value {
    match case {
        0 => {
            json!({"tenant_id":id,"revision":1,"limits":{"concurrent_executions":1,"requests_per_minute":10,"resource_pools":[]}})
        }
        1 => json!({"resource_pool_id":id,"revision":1,"concurrent_attempts":1}),
        2 => {
            json!({"principal_id":id,"tenant_id":"tenant","authorization_revision":"revision","active":true,"expires_at_ms":null,"requests_per_minute":null,"policy_id":null})
        }
        3 => json!({"provider":id,"account":"ac &/é","resource_pool_id":"pool","version":1}),
        _ => {
            json!({"execution_id":id,"correlation_id":"correlation","revision":1,"operation":"generation","config_generation":1,"accepted_at_ms":42,"updated_at_ms":42,"state":{"status":"accepted"},"attempts":[]})
        }
    }
}

async fn read_metadata(
    client: &OperatorClient,
    case: usize,
    id: &str,
) -> std::result::Result<(), Error> {
    match case {
        0 => {
            client.tenant(id).await?;
        }
        1 => {
            client.resource(id).await?;
        }
        2 => {
            client.principal(id).await?;
        }
        3 => {
            client.credential(id, "ac &/é").await?;
        }
        _ => {
            client.execution(id).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn metadata_preserves_query_values_and_rejects_identity_mismatch() -> Result {
    const ID: &str = "tenant/東京 %&+?";
    for case in 0..5 {
        for matches in [true, false] {
            let server = serve(vec![Reply {
                status: 200,
                body: metadata(case, if matches { ID } else { "wrong" }),
                location: None,
            }])
            .await?;
            let result =
                read_metadata(&OperatorClient::new(&server.endpoint, TOKEN)?, case, ID).await;
            let requests = server.requests.lock().await;
            assert_eq!(requests.len(), 1);
            let uri = url::Url::parse(&format!("http://localhost{}", requests[0].uri))?;
            let query = uri
                .query_pairs()
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                query
                    .get(if case == 3 { "provider" } else { "id" })
                    .map(|value| value.as_ref()),
                Some(ID)
            );
            assert_eq!(query.len(), if case == 3 { 2 } else { 1 });
            if case == 3 {
                assert_eq!(
                    query.get("account").map(|value| value.as_ref()),
                    Some("ac &/é")
                );
            }
            drop(requests);
            server.finish().await?;
            if matches {
                result?;
            } else {
                assert!(matches!(result, Err(Error::Response(_))));
            }
        }
    }
    let mut wrong_receipt = receipt();
    wrong_receipt["operation_id"] = json!("other");
    for body in [
        json!({"status":"unresolved","operation_id":"other"}),
        json!({"status":"committed","receipt":wrong_receipt}),
    ] {
        let server = serve(vec![Reply {
            status: 200,
            body,
            location: None,
        }])
        .await?;
        let result = OperatorClient::new(&server.endpoint, TOKEN)?
            .operation("..")
            .await;
        server.finish().await?;
        assert!(matches!(result, Err(Error::Response(_))));
    }
    Ok(())
}

#[test]
fn operator_token_grammar_excludes_merged_or_quoted_credentials() {
    for suffix in [",other", "\"quote", "\\escape", "=middle=x"] {
        assert!(OperatorClient::new("http://localhost", &format!("{TOKEN}{suffix}")).is_err());
    }
    assert!(OperatorClient::new("http://localhost", &format!("{TOKEN}+/==")).is_ok());
}
