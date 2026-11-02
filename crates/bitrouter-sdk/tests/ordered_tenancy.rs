//! Ordered v2 transport contracts over localhost HTTP/1.1 and prior-knowledge h2c.

use std::{error::Error as StdError, time::Duration};

use axum::{body::Body as HttpBody, http::Version, response::Response};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    execution_v2::ExecutionReceiptV2,
    ordered::{
        GenerationProtocol, OrderedCandidateSelectionV1, OrderedCapabilitiesV2,
        OrderedDispatchBindingV2,
    },
    usage_evidence_v2::ExecutionUsageEvidenceV2,
    validation::Validate,
};
use bitrouter_sdk::{
    gateway::{GatewayClient, HttpVersion, TransportOptions},
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};

#[path = "ordered_tenancy/control.rs"]
mod control;
#[path = "ordered_tenancy/evidence.rs"]
mod evidence_read;
#[path = "ordered_tenancy/gateway.rs"]
mod gateway;
#[path = "transport_protocol/server.rs"]
mod server;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TIMEOUT: Duration = Duration::from_secs(5);

fn versions() -> Vec<(HttpVersion, Version)> {
    vec![
        (HttpVersion::Http1Only, Version::HTTP_11),
        #[cfg(feature = "http2")]
        (HttpVersion::Http2PriorKnowledge, Version::HTTP_2),
    ]
}

fn capabilities(limit: u8) -> TestResult<OrderedCapabilitiesV2> {
    Ok(serde_json::from_value(
        json!({"contract_version":2,"installation_id":"installation",
        "instance_id":"instance","capability_revision":"capability","authority_revision":7,
        "attempt_limit":limit,"revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true}),
    )?)
}
fn route() -> TestResult<ResourceRef> {
    Ok(serde_json::from_value(
        json!({"owner_kind":"tenant","owner_id":"tenant",
        "resource_kind":"route","resource_id":"route","revision_id":"route-revision"}),
    )?)
}
fn selection(indices: Vec<u16>) -> TestResult<OrderedCandidateSelectionV1> {
    Ok(OrderedCandidateSelectionV1 {
        generation_defaults_digest: None,
        admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
        schema_version: 1,
        route: route()?,
        candidate_indices: indices,
    })
}
fn binding(indices: Vec<u16>) -> TestResult<OrderedDispatchBindingV2> {
    Ok(capabilities(64)?.bind(
        "revision".try_into()?,
        "correlation".try_into()?,
        selection(indices)?,
    )?)
}
fn gateway_client(server: &server::Server, version: HttpVersion) -> TestResult<GatewayClient> {
    Ok(GatewayClient::with_options(
        &format!("{}/prefix/v1", server.endpoint),
        Some("default-token"),
        TransportOptions {
            http_version: version,
            connect_timeout: TIMEOUT,
            read_timeout: Some(TIMEOUT),
            request_timeout: Some(TIMEOUT),
            ..Default::default()
        },
    )?)
}
fn control_client(server: &server::Server, version: HttpVersion) -> TestResult<TenancyClient> {
    Ok(TenancyClient::with_options(
        &format!("{}/control/v1", server.endpoint),
        "operator-token",
        TenancyOptions {
            http_version: version,
            connect_timeout: TIMEOUT,
            request_timeout: TIMEOUT,
            ..Default::default()
        },
    )?)
}
fn error_body() -> Value {
    json!({"code":"capability_mismatch","message":"unsupported version","dispatch_state":"not_started","operation_id":null,"execution_id":null})
}
fn json_status(status: u16, value: Value) -> TestResult<Response> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(HttpBody::from(value.to_string()))?)
}
fn unknown_usage() -> Value {
    json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,
        "reasoning_tokens":null,"provenance":"unknown","complete":false})
}
fn receipt(count: u16) -> TestResult<ExecutionReceiptV2> {
    let value = serde_json::from_value::<ExecutionReceiptV2>(
        json!({"schema_version":2,"installation_id":"installation",
        "tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation",
        "execution_revision":"revision","accepted_selection":selection((0..count).collect())?.accept(GenerationProtocol::ChatCompletions)?,
        "attempt_limit":64,"state":"committed","outcome":"failed","attempts":(0..count).map(|index| json!({
            "attempt_id":format!("attempt-{index}"),"candidate_index":index,"target":null,"outcome":"failed",
            "observed_http_status":429,"usage":unknown_usage(),"native_usage":[]
        })).collect::<Vec<_>>(),"usage":unknown_usage(),"recorded_at":"2026-11-02T00:00:00Z"}),
    )?;
    value.validate()?;
    Ok(value)
}
fn evidence(receipt: &ExecutionReceiptV2) -> TestResult<ExecutionUsageEvidenceV2> {
    let value = serde_json::from_value::<ExecutionUsageEvidenceV2>(
        json!({"schema_version":2,"installation_id":receipt.installation_id,
        "tenant_id":receipt.tenant_id,"execution_id":receipt.execution_id,"accepted_selection":receipt.accepted_selection,
        "capture":"captured","extractor_version":1,"estimation":"not_captured","estimate":null,
        "attempts":receipt.attempts.iter().map(|attempt| json!({"attempt_id":attempt.attempt_id,"target":attempt.target,
            "outcome":attempt.outcome,"terminal_observed":false})).collect::<Vec<_>>(),
        "frames":[],"truncated":false,"dropped_frames":0}),
    )?;
    value.validate_against(receipt)?;
    Ok(value)
}
