//! HTTP transport for the generic eval exchange.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use serde::Deserialize;

use super::EvalService;
use super::admission::SubmissionPrincipal;
use super::types::{EvalSubject, EvaluationResult};
use crate::auth::hook::credential_from_headers;
use crate::auth::{db as auth_db, keys};

#[derive(Clone)]
struct ApiState {
    service: EvalService,
    db: DatabaseConnection,
    skip_auth: bool,
}

/// Mount the evaluator-neutral exchange. These endpoints only mutate the
/// evidence ledger; they never publish or edit a policy lock.
pub fn router(service: EvalService, db: DatabaseConnection, skip_auth: bool) -> Router {
    let state = ApiState {
        service,
        db,
        skip_auth,
    };
    Router::new()
        .route("/v1/evals/subjects", post(put_subject).get(list_subjects))
        .route("/v1/evals/subjects/{eval_id}", get(get_subject))
        .route("/v1/evals/results", post(submit_result))
        .route("/v1/evals/snapshots", post(freeze_snapshot))
        .route("/v1/evals/snapshots/{evidence_root}", get(get_snapshot))
        .route("/v1/evals/status", get(status))
        .with_state(state)
}

async fn put_subject(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(subject): Json<EvalSubject>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let outcome = state
        .service
        .store()
        .insert_subject_owned(&subject, principal.owner_user_id())
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(serde_json::json!({
        "eval_id": subject.eval_id,
        "outcome": format!("{outcome:?}").to_ascii_lowercase(),
    })))
}

async fn list_subjects(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<Vec<EvalSubject>>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    Ok(Json(
        state
            .service
            .store()
            .list_subjects_for_owner(principal.owner_user_id())
            .await
            .map_err(ApiError::internal)?,
    ))
}

async fn get_subject(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(eval_id): Path<String>,
) -> Result<Json<EvalSubject>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let subject = state
        .service
        .store()
        .subject_for_owner(&eval_id, principal.owner_user_id())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("eval subject not found"))?;
    Ok(Json(subject))
}

async fn submit_result(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(result): Json<EvaluationResult>,
) -> Result<Json<super::admission::AdmissionOutcome>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let outcome = state
        .service
        .submit(result, principal)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(outcome))
}

#[derive(Debug, Deserialize)]
struct FreezeRequest {
    frozen_at: Option<String>,
}

async fn freeze_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    payload: Option<Json<FreezeRequest>>,
) -> Result<Json<super::store::EvalSnapshot>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let frozen_at = payload
        .and_then(|Json(request)| request.frozen_at)
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    Ok(Json(
        state
            .service
            .store()
            .freeze_snapshot_for_owner(&frozen_at, principal.owner_user_id())
            .await
            .map_err(ApiError::bad_request)?,
    ))
}

async fn get_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(evidence_root): Path<String>,
) -> Result<Json<super::store::EvalSnapshot>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let snapshot = state
        .service
        .store()
        .snapshot_by_root_for_owner(&evidence_root, principal.owner_user_id())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("eval snapshot not found"))?;
    Ok(Json(snapshot))
}

async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let principal = authenticate(&state, &headers).await?;
    let subjects = state
        .service
        .store()
        .list_subjects_for_owner(principal.owner_user_id())
        .await
        .map_err(ApiError::internal)?;
    let admissions = state
        .service
        .store()
        .latest_admissions_for_owner(principal.owner_user_id())
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({
        "subjects": subjects.len(),
        "results": admissions.len(),
        "admission": admissions.values().fold(
            std::collections::BTreeMap::<String, usize>::new(),
            |mut counts, event| {
                *counts.entry(format!("{:?}", event.status).to_ascii_lowercase()).or_default() += 1;
                counts
            }
        ),
    })))
}

async fn authenticate(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<SubmissionPrincipal, ApiError> {
    if state.skip_auth {
        return Ok(SubmissionPrincipal::LocalOperator);
    }
    let credential = credential_from_headers(headers)
        .ok_or_else(|| ApiError::unauthorized("missing API key"))?;
    if !keys::looks_like_virtual_key(&credential) {
        return Err(ApiError::unauthorized(
            "credential is not a brvk_ virtual key",
        ));
    }
    let record = auth_db::find_key_by_hash(&state.db, &keys::hash_key(&credential))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::unauthorized("unknown API key"))?;
    if !record.active || record.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        return Err(ApiError::unauthorized("API key is inactive or expired"));
    }
    Ok(SubmissionPrincipal::ApiKey {
        key_id: record.id,
        user_id: record.user_id,
    })
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use axum_test::TestServer;
    use bitrouter_sdk::config::EvalConfig;
    use sea_orm::DatabaseConnection;

    use super::router;
    use crate::auth::{db as auth_db, keys};
    use crate::eval::EvalService;
    use crate::eval::store::EvalStore;
    use crate::eval::types::{
        EVAL_SCHEMA_VERSION, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
        EvaluatorIdentity, EvaluatorKind, evidence_digest,
    };

    #[tokio::test]
    async fn local_http_exchange_round_trips_a_subject() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let service = EvalService::new(EvalStore::new(db.clone()), EvalConfig::default());
        let server = TestServer::new(router(service, db, true));
        let subject = subject()?;

        server
            .post("/v1/evals/subjects")
            .json(&subject)
            .await
            .assert_status_ok();
        let fetched = server
            .get("/v1/evals/subjects/eval-http")
            .await
            .json::<EvalSubject>();
        assert_eq!(fetched, subject);
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_exchange_is_tenant_scoped() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let user_a_key = insert_active_key(&db, "user-a").await?;
        let user_b_key = insert_active_key(&db, "user-b").await?;
        let service = EvalService::new(EvalStore::new(db.clone()), EvalConfig::default());
        let server = TestServer::new(router(service, db, false));
        let mut subject_a = subject()?;
        subject_a.eval_id = "eval-user-a".into();
        subject_a.subject_id = "task-user-a".into();
        let mut subject_b = subject()?;
        subject_b.eval_id = "eval-user-b".into();
        subject_b.subject_id = "task-user-b".into();

        server
            .post("/v1/evals/subjects")
            .authorization_bearer(&user_a_key)
            .json(&subject_a)
            .await
            .assert_status_ok();
        server
            .post("/v1/evals/subjects")
            .authorization_bearer(&user_b_key)
            .json(&subject_b)
            .await
            .assert_status_ok();

        let user_a_subjects = server
            .get("/v1/evals/subjects")
            .authorization_bearer(&user_a_key)
            .await
            .json::<Vec<EvalSubject>>();
        assert_eq!(user_a_subjects, vec![subject_a.clone()]);
        server
            .get("/v1/evals/subjects/eval-user-a")
            .authorization_bearer(&user_b_key)
            .await
            .assert_status_not_found();
        server
            .post("/v1/evals/results")
            .authorization_bearer(&user_b_key)
            .json(&EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: "eval-user-a".into(),
                evidence_digest: subject_a.evidence_digest.clone(),
                evaluator: EvaluatorIdentity {
                    authority_id: "user-b-evaluator".into(),
                    evaluator_id: "cross-tenant-probe".into(),
                    kind: EvaluatorKind::Generic,
                    version: "1".into(),
                    config_digest: subject_a.policy_digest.clone(),
                },
                verdict: EvalVerdict::Pass,
                metrics: BTreeMap::new(),
                hard_violations: Vec::new(),
                confidence_ppm: None,
                evidence_refs: Vec::new(),
                decision_credit: BTreeMap::new(),
                idempotency_key: "cross-tenant-result".into(),
                submitted_at: "2026-07-30T00:01:00Z".into(),
            })
            .await
            .assert_status_bad_request();

        let snapshot_a = server
            .post("/v1/evals/snapshots")
            .authorization_bearer(&user_a_key)
            .json(&serde_json::json!({ "frozen_at": "2026-07-30T00:02:00Z" }))
            .await
            .json::<crate::eval::store::EvalSnapshot>();
        let snapshot_b = server
            .post("/v1/evals/snapshots")
            .authorization_bearer(&user_b_key)
            .json(&serde_json::json!({ "frozen_at": "2026-07-30T00:02:00Z" }))
            .await
            .json::<crate::eval::store::EvalSnapshot>();
        assert_ne!(snapshot_a.evidence_root, snapshot_b.evidence_root);
        server
            .get(&format!("/v1/evals/snapshots/{}", snapshot_a.evidence_root))
            .authorization_bearer(&user_b_key)
            .await
            .assert_status_not_found();
        Ok(())
    }

    async fn insert_active_key(db: &DatabaseConnection, user_id: &str) -> anyhow::Result<String> {
        auth_db::upsert_user(db, user_id).await?;
        let key = keys::generate();
        auth_db::insert_api_key(
            db,
            &auth_db::NewApiKey {
                id: format!("key-{user_id}"),
                key_hash: key.hash,
                user_id: user_id.to_string(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        Ok(key.secret)
    }

    fn subject() -> anyhow::Result<EvalSubject> {
        let evidence = Vec::new();
        Ok(EvalSubject {
            schema_version: 1,
            eval_id: "eval-http".into(),
            scope: EvalScope::Task,
            subject_id: "task-http".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: Vec::new(),
            requested_dimensions: BTreeSet::from(["quality.pass".into()]),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "2026-07-30T00:00:00Z".into(),
        })
    }
}
