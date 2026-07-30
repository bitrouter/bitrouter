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
    authenticate(&state, &headers).await?;
    let outcome = state
        .service
        .store()
        .insert_subject(&subject)
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
    authenticate(&state, &headers).await?;
    Ok(Json(
        state
            .service
            .store()
            .list_subjects()
            .await
            .map_err(ApiError::internal)?,
    ))
}

async fn get_subject(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(eval_id): Path<String>,
) -> Result<Json<EvalSubject>, ApiError> {
    authenticate(&state, &headers).await?;
    let subject = state
        .service
        .store()
        .subject(&eval_id)
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
    authenticate(&state, &headers).await?;
    let frozen_at = payload
        .and_then(|Json(request)| request.frozen_at)
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    Ok(Json(
        state
            .service
            .store()
            .freeze_snapshot(&frozen_at)
            .await
            .map_err(ApiError::bad_request)?,
    ))
}

async fn get_snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(evidence_root): Path<String>,
) -> Result<Json<super::store::EvalSnapshot>, ApiError> {
    authenticate(&state, &headers).await?;
    let snapshot = state
        .service
        .store()
        .snapshot_by_root(&evidence_root)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("eval snapshot not found"))?;
    Ok(Json(snapshot))
}

async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authenticate(&state, &headers).await?;
    let subjects = state
        .service
        .store()
        .list_subjects()
        .await
        .map_err(ApiError::internal)?;
    let admissions = state
        .service
        .store()
        .latest_admissions()
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

fn credential_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth).trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
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
    use std::collections::BTreeSet;

    use axum_test::TestServer;
    use bitrouter_sdk::config::EvalConfig;

    use super::router;
    use crate::eval::EvalService;
    use crate::eval::store::EvalStore;
    use crate::eval::types::{EvalScope, EvalSubject, evidence_digest};

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
