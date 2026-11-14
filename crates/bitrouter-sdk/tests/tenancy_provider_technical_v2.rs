//! Core GET only: no provider transport or background replay.
#[path = "transport_protocol/server.rs"]
mod server;
use axum::{body::Body, http::Version, response::Response};
use bitrouter_sdk::{error::Error, tenancy::TenancyClient};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "scope-reader-fixture-token-0123456789";
fn facts() -> Value {
    json!({"schema_version":2,"receipt_format":1,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision",
 "operations":[{"attempt_ids":["attempt"],"selected_attempt_id":"attempt","streamed":false,"upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":false}],
 "pipeline_scope":{"profile":"core_pipeline_finalized_v1","context_request_id":"actual-context-not-correlation","request_duration_ms":1,"streamed":false,"stream":null},"hop_observations":[]})
}
#[tokio::test]
async fn actual_core_v2_get_preserves_zero_null_context_and_reuses_existing_connection()
-> TestResult {
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(Value::Null)?),
            Some(server::json(facts())?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let installation = "store".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    assert!(
        client
            .provider_technical_facts_v2(&installation, &tenant, &execution)
            .await?
            .is_none()
    );
    let value = client
        .provider_technical_facts_v2(&installation, &tenant, &execution)
        .await?
        .ok_or("scope")?;
    assert_eq!(value.operations[0].upstream_duration_ms, Some(0));
    let scope = value.pipeline_scope.ok_or("pipeline scope")?;
    assert_eq!(scope.request_duration_ms, 1);
    assert_eq!(scope.context_request_id, "actual-context-not-correlation");
    assert!(scope.stream.is_none());
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    let connection = records[0].connection;
    assert!(connection > 0);
    for record in records.iter() {
        assert_eq!(record.version, Version::HTTP_11);
        assert_eq!(
            record.connection, connection,
            "fully consumed sequential Core GETs reuse connection"
        );
        assert_eq!(record.method, "GET");
        assert!(record.body.is_empty());
        assert_eq!(record.headers["bitrouter-installation-id"], "store");
        assert!(
            record
                .path
                .ends_with("/tenants/tenant/executions/execution/provider-technical-facts/v2")
        );
    }
    Ok(())
}
#[tokio::test]
async fn foreign_unsupported_old_or_malformed_scope_never_falls_back_or_retries() -> TestResult {
    for case in [
        "unsupported",
        "old",
        "foreign",
        "missing",
        "duplicate",
        "array",
        "overflow",
        "large",
    ] {
        let mut value = facts();
        match case {
            "old" => value["schema_version"] = json!(1),
            "foreign" => value["execution_id"] = json!("other"),
            "missing" => {
                value["pipeline_scope"]
                    .as_object_mut()
                    .ok_or("scope map")?
                    .remove("streamed");
            }
            "array" => {
                value["pipeline_scope"] =
                    json!(["core_pipeline_finalized_v1", "context", 0, false, null])
            }
            _ => {}
        }
        let mut raw = value.to_string();
        if case == "duplicate" {
            raw = raw.replace(
                "\"schema_version\":2",
                "\"schema_version\":2,\"schema_version\":2",
            );
        }
        if case == "overflow" {
            raw = raw.replace(
                "\"request_duration_ms\":1",
                "\"request_duration_ms\":18446744073709551616",
            );
        }
        if case == "large" {
            raw = " ".repeat(65537) + &raw;
        }
        let response = Response::builder()
            .status(if case == "unsupported" { 404 } else { 200 })
            .header("content-type", "application/json")
            .body(Body::from(raw))?;
        let server = server::serve(Version::HTTP_11, vec![Some(response)]).await?;
        let client = TenancyClient::new(&server.endpoint, TOKEN)?;
        let result = client
            .provider_technical_facts_v2(
                &"store".try_into()?,
                &"tenant".try_into()?,
                &"execution".try_into()?,
            )
            .await;
        assert!(result.is_err(), "{case}");
        if case == "unsupported" {
            assert!(matches!(result,Err(Error::Tenancy(error)) if error.status==404));
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1, "{case}");
        assert_eq!(records[0].version, Version::HTTP_11);
        assert_eq!(records[0].connection, 1);
    }
    Ok(())
}
