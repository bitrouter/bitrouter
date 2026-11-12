//! Existing real Core HTTP transport only; this fixture never sends to providers.
#[path = "transport_protocol/server.rs"]
mod server;
use axum::{body::Body, http::Version, response::Response};
use bitrouter_sdk::{error::Error, tenancy::TenancyClient};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "technical-reader-fixture-token-0123456789";
fn facts() -> Value {
    json!({"schema_version":1,"receipt_format":1,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision",
        "operations":[{"attempt_ids":["attempt"],"selected_attempt_id":"attempt","streamed":false,"upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":false}]})
}
fn legacy_receipt() -> Value {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    json!({"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed",
        "attempts":[{"attempt_id":"attempt","target":null,"outcome":"failed","usage":usage,"native_usage":[]}],"usage":usage,"recorded_at":"2026-01-01T00:00:00Z"})
}
#[tokio::test]
async fn null_zero_facts_and_old_receipt_use_only_original_get_transport() -> TestResult {
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(Value::Null)?),
            Some(server::json(facts())?),
            Some(server::json(legacy_receipt())?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let install = "store".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    assert!(
        client
            .provider_technical_facts_v1(&install, &tenant, &execution)
            .await?
            .is_none()
    );
    let facts = client
        .provider_technical_facts_v1(&install, &tenant, &execution)
        .await?
        .ok_or("facts")?;
    assert_eq!(facts.operations[0].upstream_duration_ms, Some(0));
    let old = client.execution(&install, &tenant, &execution).await?;
    facts.validate_against(&old)?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 3);
    let connection = records[0].connection;
    assert!(connection > 0);
    for record in records.iter() {
        assert_eq!(record.version, Version::HTTP_11);
        assert_eq!(
            record.connection, connection,
            "sequential consumed GETs must reuse the original Core connection"
        );
        assert_eq!(record.method, "GET");
        assert!(record.body.is_empty());
        assert_eq!(record.headers["bitrouter-installation-id"], "store");
    }
    assert!(
        records[0]
            .path
            .ends_with("/tenants/tenant/executions/execution/provider-technical-facts/v1")
    );
    assert!(
        records[2]
            .path
            .ends_with("/tenants/tenant/executions/execution")
    );
    Ok(())
}
#[tokio::test]
async fn unsupported_cross_identity_oversize_bad_schema_and_duplicate_never_retry() -> TestResult {
    for case in [
        "unsupported",
        "foreign",
        "large",
        "schema",
        "duplicate",
        "fraction",
    ] {
        let mut value = facts();
        if case == "foreign" {
            value["tenant_id"] = json!("foreign");
        }
        if case == "schema" {
            value["schema_version"] = json!(2);
        }
        let mut raw = value.to_string();
        if case == "duplicate" {
            raw = raw.replace(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
            );
        }
        if case == "fraction" {
            raw = raw.replace("\"upstream_duration_ms\":0", "\"upstream_duration_ms\":0.1");
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
            .provider_technical_facts_v1(
                &"store".try_into()?,
                &"tenant".try_into()?,
                &"execution".try_into()?,
            )
            .await;
        assert!(result.is_err(), "{case}");
        if case == "unsupported" {
            assert!(matches!(result,Err(Error::Tenancy(failure)) if failure.status==404));
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1, "{case}");
        assert_eq!(records[0].version, Version::HTTP_11, "{case}");
        assert_eq!(
            records[0].connection, 1,
            "single GET uses the first accepted Core connection: {case}"
        );
    }
    Ok(())
}
