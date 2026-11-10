use super::*;
use crate::gateway::{
    caller::CallerContext,
    pipeline::{context::PipelineContext, types::PipelineRequest},
    scheduling::payload::{Weight, compact_dto},
};
use bitrouter_ai::event::PipelineEvent;
use serde::Serialize;

#[test]
fn scheduling_payload_charges_spare_prompt_and_context_storage() -> Result<()> {
    let mut prompt = prompt(false)?;
    prompt.model.reserve(65536);
    prompt.messages.reserve(1024);
    let owned = prompt.model.capacity()
        + prompt.messages.capacity() * std::mem::size_of::<bitrouter_ai::types::Message>();
    let mut weight = Weight::new();
    weight.prompt(&mut prompt)?;
    ensure!(weight.total() >= owned as u64);
    let mut request = PipelineRequest::new("model", CallerContext::local(), prompt);
    let mut header = http::HeaderValue::from_static("opaque");
    header.set_sensitive(true);
    request.headers.append("x-test", header);
    request.headers.reserve(1024);
    let mut ctx = PipelineContext::new(request);
    ctx.enable_scheduling_accounting();
    let bytes = ctx.prepare_scheduling_payload()?;
    ensure!(bytes >= 2 * owned as u64);
    ensure!(ctx.headers().capacity() < 1024);
    ensure!(ctx.headers()["x-test"].is_sensitive());
    Ok(())
}

#[test]
fn scheduling_payload_compacts_hidden_json_map_capacity_without_changing_values() -> Result<()> {
    let mut map = serde_json::Map::new();
    for i in 0..10000 {
        map.insert(i.to_string(), serde_json::Value::Null);
    }
    map.clear();
    map.insert(
        "retained".into(),
        serde_json::json!({"items":[1,"value",true]}),
    );
    let mut value = serde_json::Value::Object(map);
    let expected = value.clone();
    let mut weight = Weight::new();
    weight.json(&mut value)?;
    ensure!(value == expected);
    ensure!(weight.total() < 4096);
    let scalar = "x".repeat(1024 * 1024);
    let bytes = compact_dto(&scalar)?;
    ensure!(bytes >= scalar.len() as u64 && bytes < scalar.len() as u64 + 1024);
    Ok(())
}

#[derive(Serialize)]
struct OpaqueEvent {
    value: String,
}
impl PipelineEvent for OpaqueEvent {
    fn event_name(&self) -> &'static str {
        "test.opaque"
    }
}

#[test]
fn scheduling_payload_rejects_opaque_ingress_extensions_and_events() -> Result<()> {
    let mut request = PipelineRequest::new("model", CallerContext::local(), prompt(false)?);
    request.extensions.insert(Arc::new(vec![0u8; 65536]));
    let mut ctx = PipelineContext::new(request);
    ctx.enable_scheduling_accounting();
    ensure!(ctx.scheduling_accounted_bytes().is_err());

    let mut ctx = PipelineContext::new(PipelineRequest::new(
        "model",
        CallerContext::local(),
        prompt(false)?,
    ));
    ctx.enable_scheduling_accounting();
    ctx.emit_accounted(
        OpaqueEvent {
            value: "known".into(),
        },
        compact_dto,
    );
    ensure!(ctx.scheduling_accounted_bytes()? > 0);
    ctx.emit(OpaqueEvent {
        value: "unknown".into(),
    });
    ensure!(ctx.scheduling_accounted_bytes().is_err());
    Ok(())
}

#[test]
fn scheduling_payload_charges_custom_protocol_route_override_and_trace_backing() -> Result<()> {
    let mut req = PipelineRequest::new("model", CallerContext::local(), prompt(false)?);
    let mut protocol = String::from("canonical");
    protocol.reserve(65536);
    let protocol_bytes = protocol.capacity();
    req.inbound_protocol = Some(bitrouter_ai::types::ApiProtocol::Custom(protocol));
    let mut ctx = PipelineContext::new(req);
    ctx.enable_scheduling_accounting();
    let mut target = target("https://fixture.invalid")?;
    target.api_key.reserve(65536);
    let target_bytes = target.api_key.capacity();
    ctx.route_chain = Some(vec![target]);
    ensure!(ctx.prepare_scheduling_payload()? >= (protocol_bytes + target_bytes) as u64);

    let backing = axum::body::Bytes::from(vec![b'x'; 1024 * 1024]);
    let mut value = http::HeaderValue::from_maybe_shared(backing.slice(0..3))?;
    value.set_sensitive(true);
    let mut headers = http::HeaderMap::new();
    headers.insert("tracestate", value.clone());
    ctx.set_outbound_trace_headers(headers);
    ensure!(ctx.scheduling_accounted_bytes()? < 4096);
    let compact = ctx.take_outbound_trace_headers().context("trace headers")?;
    ensure!(compact["tracestate"] == value && compact["tracestate"].is_sensitive());
    ensure!(compact["tracestate"].as_bytes().as_ptr() != value.as_bytes().as_ptr());
    Ok(())
}

#[test]
fn scheduling_payload_measures_retained_fallback_errors_in_place() -> Result<()> {
    let mut text = String::from("small");
    text.reserve(65536);
    let mut values = Vec::with_capacity(4096);
    let retained = text.capacity() + values.capacity() * std::mem::size_of::<serde_json::Value>();
    values.push(serde_json::Value::String(text));
    let error = serde_json::Value::Array(values);
    let expected = error.clone();
    let mut errors = [crate::gateway::error::BitrouterError::UpstreamBadRequest { error }];
    ensure!(crate::gateway::scheduling::payload::errors(&mut errors)? >= retained as u64);
    ensure!(
        matches!(&errors[0], crate::gateway::error::BitrouterError::UpstreamBadRequest { error } if error == &expected)
    );

    let mut map = serde_json::Map::new();
    for i in 0..10000 {
        map.insert(i.to_string(), serde_json::Value::Null);
    }
    map.clear();
    let mut errors = [crate::gateway::error::BitrouterError::UpstreamBadRequest {
        error: serde_json::Value::Object(map),
    }];
    ensure!(crate::gateway::scheduling::payload::errors(&mut errors)? < 1024);
    ensure!(
        matches!(&errors[0], crate::gateway::error::BitrouterError::UpstreamBadRequest { error } if error == &serde_json::json!({}))
    );
    Ok(())
}
