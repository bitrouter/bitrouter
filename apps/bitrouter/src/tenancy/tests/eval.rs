//! Owned ledger tests through the mounted protected control router.
use super::*;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use bitrouter_ai::eval::{
    AdmissionOutcome, EvalSnapshot, EvalSubject, EvalVerdict, EvaluationResult, PutSubjectReport,
    StatusReport,
};
use std::time::Duration;
use tower::ServiceExt;

mod authority;
mod custody;
mod sdk;

const TIMEOUT: Duration = Duration::from_secs(10);
fn router(fixture: &Fixture) -> Result<Router> {
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "TENANCY_EVAL_TEST_OPERATOR".into(),
                scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some("owned-eval-test-operator-token-at-least-32".into()),
    )?;
    Ok(crate::tenancy::http::router(fixture.service.clone(), auth))
}
fn subject() -> Result<EvalSubject> {
    Ok(serde_json::from_value(serde_json::json!({
        "schema_version":1,"eval_id":"owned-subject","scope":"request","subject_id":"request-1",
        "policy_digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "holdout":false,"decisions":[],"requested_dimensions":[],"evidence":[],
        "evidence_digest":crate::storage::evaluation::evidence_digest(&[])?,"observed_at":"2026-10-25T00:00:00Z"
    }))?)
}
fn evaluation(fixture: &Fixture, verdict: EvalVerdict, key: &str) -> Result<EvaluationResult> {
    let subject = subject()?;
    Ok(serde_json::from_value(serde_json::json!({
        "schema_version":1,"eval_id":subject.eval_id,"evidence_digest":subject.evidence_digest,
        "evaluator":{"authority_id":fixture.principal,"evaluator_id":"generic-fixture","kind":"generic","version":"1","config_digest":subject.policy_digest},
        "verdict":verdict,"metrics":{},"hard_violations":[],"evidence_refs":[],"decision_credit":{},"idempotency_key":key,"submitted_at":"2026-10-25T00:01:00Z"
    }))?)
}
fn request(
    fixture: &Fixture,
    secret: &str,
    method: &str,
    suffix: &str,
    body: Vec<u8>,
) -> Result<Request<Body>> {
    Ok(Request::builder()
        .method(method)
        .uri(format!(
            "/control/v1/tenants/{}/evals/{suffix}",
            fixture.tenant
        ))
        .header("authorization", format!("Bearer {secret}"))
        .header("bitrouter-installation-id", fixture.installation.as_str())
        .header("content-type", "application/json")
        .body(Body::from(body))?)
}
async fn json<T: serde::de::DeserializeOwned>(response: axum::response::Response) -> Result<T> {
    ensure!(
        response.status() == StatusCode::OK,
        "unexpected status: {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get("cache-control")
            .is_some_and(|value| value == "no-store")
    );
    Ok(serde_json::from_slice(
        &tokio::time::timeout(
            TIMEOUT,
            axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024),
        )
        .await??,
    )?)
}

#[tokio::test]
async fn protected_ledger_roundtrips_all_seven_routes_and_preserves_legacy_namespace() -> Result<()>
{
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("owned-eval").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        let prior_revision = fixture.service.current().map_err(failure)?.revision;
        let put: PutSubjectReport = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "subjects",
                    serde_json::to_vec(&subject()?)?,
                )?)
                .await?,
        )
        .await?;
        ensure!(put.eval_id == "owned-subject");
        let found: EvalSubject = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "GET",
                    "subjects/owned-subject",
                    Vec::new(),
                )?)
                .await?,
        )
        .await?;
        ensure!(found == subject()?);
        let listed: Vec<EvalSubject> = json(
            router
                .clone()
                .oneshot(request(&fixture, secret, "GET", "subjects", Vec::new())?)
                .await?,
        )
        .await?;
        ensure!(listed == vec![found]);
        let input = evaluation(&fixture, EvalVerdict::Pass, "pass")?;
        let admitted: AdmissionOutcome = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&input)?,
                )?)
                .await?,
        )
        .await?;
        ensure!(
            !admitted.duplicate && admitted.status == bitrouter_ai::eval::AdmissionStatus::Admitted
        );
        let replay: AdmissionOutcome = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&input)?,
                )?)
                .await?,
        )
        .await?;
        ensure!(replay.duplicate && replay.result_id == admitted.result_id);
        let frozen: EvalSnapshot = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "snapshots",
                    br#"{"frozen_at":"2026-10-25T08:02:00.000+08:00"}"#.to_vec(),
                )?)
                .await?,
        )
        .await?;
        ensure!(frozen.entries.len() == 1);
        ensure!(frozen.frozen_at == "2026-10-25T08:02:00.000+08:00");
        let stored: EvalSnapshot = json(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "GET",
                    &format!("snapshots/{}", frozen.evidence_root),
                    Vec::new(),
                )?)
                .await?,
        )
        .await?;
        ensure!(stored == frozen);
        let status: StatusReport = json(
            router
                .clone()
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?,
        )
        .await?;
        ensure!(
            status.subjects == 1
                && status.results == 1
                && status.admission.get("admitted") == Some(&1)
        );
        ensure!(fixture.service.current().map_err(failure)?.revision == prior_revision);
        ensure!(
            crate::core::eval::store::EvalStore::new(fixture.db.clone())
                .list_subjects()
                .await?
                .is_empty()
        );
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn protected_ledger_rejects_unbound_duplicate_malformed_and_forged_profile() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("eval-validation").await?;
        let secret = issued.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        for body in [
            b"{broken".to_vec(),
            b" ".to_vec(),
            br#"{"unknown":true}"#.to_vec(),
            br#"{"frozen_at":null,"frozen_at":null}"#.to_vec(),
        ] {
            let response = router
                .clone()
                .oneshot(request(&fixture, secret, "POST", "snapshots", body)?)
                .await?;
            ensure!(response.status() == StatusCode::BAD_REQUEST);
            ensure!(
                response
                    .headers()
                    .get("cache-control")
                    .is_some_and(|value| value == "no-store")
            );
        }
        let mut missing = request(&fixture, secret, "POST", "subjects", Vec::new())?;
        missing.headers_mut().remove("bitrouter-installation-id");
        ensure!(router.clone().oneshot(missing).await?.status() == StatusCode::BAD_REQUEST);
        let mut duplicate = request(&fixture, secret, "GET", "status", Vec::new())?;
        duplicate
            .headers_mut()
            .append("authorization", format!("Bearer {secret}").parse()?);
        ensure!(router.clone().oneshot(duplicate).await?.status() == StatusCode::BAD_REQUEST);
        let nested = br#"{"evaluator":{"authority_id":"a","authority_id":"b"}}"#.to_vec();
        ensure!(
            router
                .clone()
                .oneshot(request(&fixture, secret, "POST", "results", nested)?)
                .await?
                .status()
                == StatusCode::BAD_REQUEST
        );
        let mut forged = evaluation(&fixture, EvalVerdict::Pass, "forged")?;
        forged.evaluator.authority_id = "other-principal".into();
        ensure!(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "results",
                    serde_json::to_vec(&forged)?
                )?)
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        let freeze_overflow = vec![b' '; 4 * 1024 + 1];
        ensure!(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "snapshots",
                    freeze_overflow
                )?)
                .await?
                .status()
                == StatusCode::PAYLOAD_TOO_LARGE
        );
        let mut headers_overflow = request(&fixture, secret, "GET", "status", Vec::new())?;
        for _ in 0..128 {
            headers_overflow
                .headers_mut()
                .append("x-correlation-id", "bounded".parse()?);
        }
        ensure!(
            router.clone().oneshot(headers_overflow).await?.status()
                == StatusCode::PAYLOAD_TOO_LARGE
        );
        let mut bytes_overflow = request(&fixture, secret, "GET", "status", Vec::new())?;
        bytes_overflow
            .headers_mut()
            .insert("x-correlation-id", "a".repeat(64 * 1024).parse()?);
        ensure!(
            router.clone().oneshot(bytes_overflow).await?.status() == StatusCode::PAYLOAD_TOO_LARGE
        );
        let oversized = vec![b' '; 1024 * 1024 + 1];
        ensure!(
            router
                .clone()
                .oneshot(request(&fixture, secret, "POST", "subjects", oversized)?)
                .await?
                .status()
                == StatusCode::PAYLOAD_TOO_LARGE
        );
        let status: StatusReport = json(
            router
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?,
        )
        .await?;
        ensure!(status.subjects == 0 && status.results == 0);
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn response_bodies_retain_ingress_capacity_and_read_only_grants_cannot_write() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let mut input = fixture.issuance("eval-viewer")?;
        input.permissions = vec![Permission::ResourceRead];
        let issued = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?,
        )?;
        let secret = issued.secret.as_deref().context("secret")?;
        let router = router(&fixture)?;
        ensure!(
            router
                .clone()
                .oneshot(request(
                    &fixture,
                    secret,
                    "POST",
                    "subjects",
                    serde_json::to_vec(&subject()?)?
                )?)
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        ensure!(
            router
                .clone()
                .oneshot(request(&fixture, secret, "POST", "snapshots", Vec::new())?)
                .await?
                .status()
                == StatusCode::FORBIDDEN
        );
        let mut held = Vec::new();
        for _ in 0..8 {
            let response = router
                .clone()
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?;
            ensure!(response.status() == StatusCode::OK);
            held.push(response);
        }
        ensure!(fixture.service.eval.leases.available_permits() == 0);
        ensure!(
            router
                .clone()
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?
                .status()
                == StatusCode::TOO_MANY_REQUESTS
        );
        drop(held);
        ensure!(fixture.service.eval.leases.available_permits() == 8);
        let _: StatusReport = json(
            router
                .oneshot(request(&fixture, secret, "GET", "status", Vec::new())?)
                .await?,
        )
        .await?;
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
