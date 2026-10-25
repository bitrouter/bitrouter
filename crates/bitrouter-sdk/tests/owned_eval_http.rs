//! Owned Eval SDK conformance using real HTTP/1.1 and optional h2c sockets.

#[path = "owned_eval_http/mod.rs"]
mod cases;
#[path = "transport_protocol/server.rs"]
mod server;

use std::{error::Error as StdError, time::Duration};

use axum::{
    body::Body,
    http::{StatusCode, Version, header},
    response::Response,
};
use bitrouter_ai::{
    eval::{EvalSubject, EvaluationResult, FreezeRequest},
    tenancy::{identity::InstallationId, identity::TenantId, operation::Error as TenancyError},
};
use bitrouter_sdk::{
    error::{Error, ServiceFailure},
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "owned-eval-socket-secret-canary-0123456789";
const TIME: &str = "2026-10-25T00:00:00Z";

fn protocols() -> Vec<(Version, HttpVersion)> {
    vec![
        (Version::HTTP_11, HttpVersion::Http1Only),
        #[cfg(feature = "http2")]
        (Version::HTTP_2, HttpVersion::Http2PriorKnowledge),
    ]
}

fn client(server: &server::Server, version: HttpVersion) -> TestResult<TenancyClient> {
    Ok(TenancyClient::with_options(
        &server.endpoint,
        TOKEN,
        TenancyOptions {
            http_version: version,
            ..Default::default()
        },
    )?)
}

fn namespace() -> TestResult<(InstallationId, TenantId)> {
    Ok(("installation".try_into()?, "tenant".try_into()?))
}

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn subject_value(id: &str) -> Value {
    json!({
        "schema_version":1,"eval_id":id,"scope":"request","subject_id":"request-1",
        "policy_digest":digest('a'),"preset":null,"cohort":null,"holdout":false,
        "decisions":[],"requested_dimensions":[],"evidence":[],
        "evidence_digest":digest('b'),"observed_at":TIME
    })
}

fn subject() -> TestResult<EvalSubject> {
    Ok(serde_json::from_value(subject_value("eval-1"))?)
}

fn result() -> TestResult<EvaluationResult> {
    Ok(serde_json::from_value(json!({
        "schema_version":1,"eval_id":"eval-1","evidence_digest":digest('b'),
        "evaluator":{"authority_id":"principal","evaluator_id":"evaluator", "kind":"generic",
            "version":"v1","config_digest":digest('c')},
        "verdict":"pass","metrics":{"task.pass":{"value":1,"unit":"boolean"}},
        "hard_violations":[],"confidence_ppm":null,"evidence_refs":[],
        "decision_credit":{},"idempotency_key":"result-1","submitted_at":TIME
    }))?)
}

fn receipt() -> Value {
    json!({"result_id":digest('d'),"status":"held_out","reason":"holdout", "duplicate":false})
}

fn snapshot() -> Value {
    json!({"evidence_root":digest('e'),"frozen_at":TIME,"entries":[{
        "result_id":digest('d'),"content_digest":digest('d'),
        "subject_content_digest":digest('f'),"eval_id":"eval-1"
    }]})
}

fn status(subjects: usize) -> Value {
    json!({"subjects":subjects,"results":0,"admission":{"heldout":0}})
}

fn rejection(code: &str) -> Value {
    json!({"code":code,"message":TOKEN,"dispatch_state":"not_started",
        "operation_id":null,"execution_id":null})
}

fn reply(status: u16, value: Value) -> TestResult<Response> {
    let mut response = server::json(value)?;
    *response.status_mut() = StatusCode::from_u16(status)?;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse()?);
    Ok(response)
}

fn response(body: Body) -> TestResult<Response> {
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)?)
}

fn unknown(error: Error) -> TestResult<Error> {
    assert_redacted(&error);
    match error {
        Error::OwnedEvalUnknown { source } => Ok(*source),
        other => Err(format!("expected unknown Eval outcome, got {other:?}").into()),
    }
}

fn failure(error: Error) -> TestResult<Box<ServiceFailure<TenancyError>>> {
    assert_redacted(&error);
    match error {
        Error::Tenancy(failure) => Ok(failure),
        other => Err(format!("expected typed tenancy failure, got {other:?}").into()),
    }
}

fn assert_redacted(error: &Error) {
    let mut source: Option<&dyn StdError> = Some(error);
    while let Some(error) = source {
        assert!(!format!("{error:?} {error}").contains(TOKEN));
        source = error.source();
    }
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Subject,
    Result,
    Freeze,
}

impl Mutation {
    async fn send(
        self,
        client: &TenancyClient,
        installation: &InstallationId,
        tenant: &TenantId,
    ) -> TestResult<bitrouter_sdk::error::Result<()>> {
        Ok(match self {
            Self::Subject => client
                .put_eval_subject(installation, tenant, &subject()?)
                .await
                .map(|_| ()),
            Self::Result => client
                .submit_eval_result(installation, tenant, &result()?)
                .await
                .map(|_| ()),
            Self::Freeze => client
                .freeze_eval_snapshot(installation, tenant, &FreezeRequest::default())
                .await
                .map(|_| ()),
        })
    }
}
