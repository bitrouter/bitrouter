//! Trusted, closed tenant vocabulary. No caller extension or error string can
//! supply an ownership identity, original ordinal, actual status or attempt ID.

use opentelemetry::{KeyValue, trace::SpanRef};

use crate::tenancy::{execution::TenantExecution, operational::Hop};

pub(super) fn scope(execution: &TenantExecution) -> Vec<KeyValue> {
    let (installation, tenant, revision) = execution.observation_scope();
    let mut attributes = vec![
        KeyValue::new(
            "bitrouter.installation_id",
            installation.as_str().to_owned(),
        ),
        KeyValue::new("bitrouter.tenant_id", tenant.as_str().to_owned()),
        KeyValue::new("bitrouter.request_id", execution.id().as_str().to_owned()),
        KeyValue::new("bitrouter.execution_revision", revision.as_str().to_owned()),
    ];
    if let Some(protocol) = execution.route().inbound_protocol() {
        attributes.push(KeyValue::new(
            "bitrouter.inbound_protocol",
            protocol.as_str().to_owned(),
        ));
    }
    attributes
}

pub(super) fn ordinal(span: &SpanRef<'_>, hop: &Hop) {
    if let Some(ordinal) = hop.ordinal {
        span.set_attribute(KeyValue::new(
            "bitrouter.candidate_index",
            i64::from(ordinal),
        ));
    }
}

pub(super) fn finish(span: &SpanRef<'_>, hop: &Hop) {
    ordinal(span, hop);
    let Some(observation) = hop.observation() else {
        return;
    };
    if let Some(attempt) = observation.attempt {
        span.set_attribute(KeyValue::new(
            "bitrouter.attempt_id",
            attempt.as_str().to_owned(),
        ));
    }
    if let Some(status) = observation.status {
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(status),
        ));
    }
    if let Some(outcome) = observation.outcome {
        span.set_attribute(KeyValue::new("bitrouter.hop_outcome", outcome));
    }
    if let Some(duration) = observation.duration_ms {
        span.set_attribute(KeyValue::new("bitrouter.hop_duration_ms", duration));
    }
}
