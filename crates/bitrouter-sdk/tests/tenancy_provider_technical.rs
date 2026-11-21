//! Core GET only: no provider transport or background replay.
#[path = "transport_protocol/server.rs"]
mod server;
use axum::{body::Body, http::Version, response::Response};
use bitrouter_sdk::{error::Error, tenancy::TenancyClient};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "scope-reader-fixture-token-0123456789";
fn facts() -> Value {
    json!({"schema_version":3,"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","execution_revision":"revision","operations":[{"attempt_ids":["attempt"],"selected_attempt_id":"attempt","streamed":false,"upstream_duration_ms":0,"first_semantic":null,"generation_duration_ms":null,"partial":false}],"pipeline_scope":{"profile":"core_pipeline_finalized_v1","context_request_id":"actual-context-not-correlation","request_duration_ms":1,"streamed":false,"stream":null},"hop_observations":[{"profile":"core_json_hop_callbacks_v1","attempt_ids":["attempt"],"closing_attempt_id":"attempt","duration_ms":8,"end":"completed","partial":false}],"observer_hops":[{"profile":"core_json_hop_observer_floor_v1","attempt_ids":["attempt"],"closing_attempt_id":"attempt","duration_ms":0,"end":"completed"}]})
}
fn receipt() -> Value {
    let usage = json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false});
    json!({"dispatch":{"mode":"strict"},"installation_id":"store","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"incomplete","outcome":"failed",
        "attempts":[{"attempt_id":"attempt","candidate_index":null,"observed_http_status":null,"target":null,"outcome":"failed","usage":usage,"native_usage":[]}],"usage":usage,"recorded_at":"2026-01-01T00:00:00Z"})
}

#[tokio::test]
async fn technical_get_keeps_distinct_observer_zero_and_scope_on_one_connection() -> TestResult {
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(Value::Null)?),
            Some(server::json(facts())?),
            Some(server::json(receipt())?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let installation = "store".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    assert!(
        client
            .provider_technical_facts(&installation, &tenant, &execution)
            .await?
            .is_none()
    );
    let value = client
        .provider_technical_facts(&installation, &tenant, &execution)
        .await?
        .ok_or("scope")?;
    assert_eq!(value.observer_hops[0].duration_ms, 0);
    assert_eq!(value.hop_observations[0].duration_ms, Some(8));
    assert_eq!(value.operations[0].upstream_duration_ms, Some(0));
    let original = client.execution(&installation, &tenant, &execution).await?;
    value.validate_against(&original)?;
    let scope = value.pipeline_scope.ok_or("pipeline scope")?;
    assert_eq!(scope.request_duration_ms, 1);
    assert_eq!(scope.context_request_id, "actual-context-not-correlation");
    assert!(scope.stream.is_none());
    let records = server.records.lock().await;
    assert_eq!(records.len(), 3);
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
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
    }
    for record in &records[..2] {
        assert!(
            record
                .path
                .ends_with("/tenants/tenant/executions/execution/provider-technical-facts")
        );
    }
    assert!(
        records[2]
            .path
            .ends_with("/tenants/tenant/executions/execution")
    );
    Ok(())
}
#[tokio::test]
async fn foreign_unsupported_old_or_malformed_observer_never_falls_back_or_retries() -> TestResult {
    for case in [
        "unsupported",
        "old",
        "old_scope",
        "receipt_format_1",
        "receipt_format_2",
        "foreign_installation",
        "foreign_tenant",
        "foreign_execution",
        "fraction",
        "observer_group",
        "missing_observer",
        "null_observer",
        "wrapped",
        "missing",
        "duplicate",
        "array",
        "overflow",
        "large",
    ] {
        let mut value = facts();
        match case {
            "old" => value["schema_version"] = json!(1),
            "old_scope" => value["schema_version"] = json!(2),
            "receipt_format_1" => value["receipt_format"] = json!(1),
            "receipt_format_2" => value["receipt_format"] = json!(2),
            "foreign_installation" => value["installation_id"] = json!("other"),
            "foreign_tenant" => value["tenant_id"] = json!("other"),
            "foreign_execution" => value["execution_id"] = json!("other"),
            "observer_group" => value["observer_hops"][0]["closing_attempt_id"] = json!("foreign"),
            "missing_observer" => {
                value
                    .as_object_mut()
                    .ok_or("facts object")?
                    .remove("observer_hops");
            }
            "null_observer" => value["observer_hops"] = Value::Null,
            "wrapped" => value = json!({"schema_version":3,"scope":value,"observer_hops":[]}),
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
        if case == "fraction" {
            raw = raw.replace("\"upstream_duration_ms\":0", "\"upstream_duration_ms\":0.1");
            assert_ne!(raw, value.to_string());
        }
        if case == "duplicate" {
            raw = raw.replace(
                "\"schema_version\":3",
                "\"schema_version\":3,\"schema_version\":3",
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
            .provider_technical_facts(
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

#[tokio::test]
async fn ordered_receipt_and_technical_facts_use_the_same_current_gets() -> TestResult {
    let mut original = receipt();
    let indices: Vec<_> = (247..=255).rev().collect();
    original["dispatch"] = json!({"mode":"ordered","attempt_limit":9,"accepted_selection":{
        "admission_mode":"unprofiled","inbound_protocol":"messages","candidate_indices":indices,
        "route":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route",
            "resource_id":"route","revision_id":"route-revision"}}});
    let mut attempts = Vec::new();
    let mut ids = Vec::new();
    for ordinal in indices {
        let mut attempt = original["attempts"][0].clone();
        let id = format!("actual-{ordinal}");
        attempt["attempt_id"] = json!(id);
        attempt["candidate_index"] = json!(ordinal);
        attempt["observed_http_status"] = json!(503);
        attempts.push(attempt);
        ids.push(id);
    }
    original["attempts"] = json!(attempts);
    let mut data = facts();
    data["operations"][0]["attempt_ids"] = json!(ids);
    data["operations"][0]["selected_attempt_id"] = json!(ids.last());
    for category in ["hop_observations", "observer_hops"] {
        data[category][0]["attempt_ids"] = json!(ids);
        data[category][0]["closing_attempt_id"] = json!(ids.last());
    }
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(data.clone())?),
            Some(server::json(original.clone())?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let installation = "store".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    let observed = client
        .provider_technical_facts(&installation, &tenant, &execution)
        .await?
        .ok_or("technical facts")?;
    let retained = client.execution(&installation, &tenant, &execution).await?;
    observed.validate_against(&retained)?;
    assert_eq!(serde_json::to_value(&observed)?, data);
    assert_eq!(serde_json::to_value(&retained)?, original);
    assert_eq!(retained.attempts.len(), 9);
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    assert!(
        records[0]
            .path
            .ends_with("/executions/execution/provider-technical-facts")
    );
    assert!(records[1].path.ends_with("/executions/execution"));
    assert_eq!(records[0].connection, records[1].connection);
    assert!(
        records
            .iter()
            .all(|record| record.method == "GET" && record.body.is_empty())
    );
    Ok(())
}
