//! Opt-in HTTP surface for native evaluation hosts. Stock `bro` never mounts it.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State, rejection::JsonRejection};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bitrouter_sdk::config::Config;
use bitrouter_sdk::error::BitrouterError;
use bitrouter_sdk::evaluation::EvaluationRequest;
use bitrouter_sdk::evaluation::pipeline::EvaluationPipeline;
use bitrouter_sdk::inference::InferenceOperation;
use bitrouter_sdk::language_model::routing::RoutingTable;

use crate::assemble::Assembled;
use crate::auth::AuthHook;

const BODY_LIMIT: usize = 16 * 1024 * 1024;
const REQUEST_ID_HEADER: &str = "x-bitrouter-request-id";

#[derive(Clone)]
struct EvaluationHttpState {
    pipeline: Option<Arc<EvaluationPipeline>>,
    auth: Arc<AuthHook>,
    skip_auth: bool,
    models: serde_json::Value,
}

/// Build the native evaluation HTTP routes for an explicitly linked host.
/// The caller omits the SDK's default `/v1/models` route before merging this
/// router, so advertised operations match the executable evaluation rail.
pub fn router(config: &Config, assembled: &Assembled) -> Router {
    let state = EvaluationHttpState {
        pipeline: assembled.evaluation_pipeline.clone(),
        auth: Arc::new(AuthHook::new(assembled.db.clone())),
        skip_auth: assembled.app.skip_auth(),
        models: model_list(config, assembled),
    };
    Router::new()
        .route("/v1/evaluate", post(evaluate))
        .route("/v1/models", get(list_models))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

async fn evaluate(
    State(state): State<EvaluationHttpState>,
    mut headers: HeaderMap,
    body: std::result::Result<Json<EvaluationRequest>, JsonRejection>,
) -> Response {
    let request_id = match request_id(&mut headers) {
        Ok(id) => id,
        Err(error) => return error_response(&error, None),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            let mut response = (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({
                    "error": {
                        "code": "invalid_evaluation_request",
                        "type": "invalid_request_error",
                        "message": "evaluation request exceeds body limit"
                    }
                })),
            )
                .into_response();
            attach_request_id(&mut response, &request_id);
            return response;
        }
        Err(_) => {
            return error_response(
                &BitrouterError::bad_request("invalid evaluation JSON request"),
                Some(&request_id),
            );
        }
    };
    if let Err(error) = request.validate() {
        return error_response(&error, Some(&request_id));
    }
    let caller = match state
        .auth
        .authenticate_evaluation(&headers, state.skip_auth)
        .await
    {
        Ok(caller) => caller,
        Err(error) => return error_response(&error, Some(&request_id)),
    };
    let Some(pipeline) = &state.pipeline else {
        return error_response(
            &BitrouterError::NotFound("no evaluation provider is active".into()),
            Some(&request_id),
        );
    };
    match pipeline
        .evaluate_with_caller(request, request_id.clone(), headers, caller)
        .await
    {
        Ok(result) => {
            let mut response = Json(result).into_response();
            attach_request_id(&mut response, &request_id);
            response
        }
        Err(error) => error_response(&error, Some(&request_id)),
    }
}

async fn list_models(State(state): State<EvaluationHttpState>, headers: HeaderMap) -> Response {
    let mut body = state.models;
    if headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("codex"))
        && let Some(object) = body.as_object_mut()
        && let Some(data) = object.get("data").cloned()
    {
        object.insert("models".into(), data);
    }
    Json(body).into_response()
}

fn request_id(headers: &mut HeaderMap) -> bitrouter_sdk::Result<String> {
    if let Some(value) = headers.get(REQUEST_ID_HEADER) {
        let id = value
            .to_str()
            .map_err(|_| BitrouterError::bad_request("invalid request id header"))?
            .trim();
        if id.is_empty() {
            return Err(BitrouterError::bad_request("empty request id header"));
        }
        return Ok(id.to_owned());
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&id) {
        headers.insert(REQUEST_ID_HEADER, value);
    }
    Ok(id)
}

fn attach_request_id(response: &mut Response, id: &str) {
    if let Ok(value) = HeaderValue::from_str(id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
}

fn error_response(error: &BitrouterError, request_id: Option<&str>) -> Response {
    let (status, code, message) = match error {
        BitrouterError::BadRequest { .. } => (
            StatusCode::BAD_REQUEST,
            "invalid_evaluation_request",
            "invalid evaluation request",
        ),
        BitrouterError::Unauthorized(_) => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid API key",
        ),
        BitrouterError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden", "access denied"),
        BitrouterError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            "evaluation_model_not_found",
            "evaluation model not found",
        ),
        BitrouterError::ModelOperationMismatch(_) => (
            StatusCode::CONFLICT,
            "model_operation_mismatch",
            "model does not support evaluation",
        ),
        BitrouterError::UpstreamBadRequest { .. }
        | BitrouterError::Upstream { status: 422, .. } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "provider_rejected_evaluation",
            "provider rejected evaluation request",
        ),
        BitrouterError::UpstreamRateLimited { .. }
        | BitrouterError::Upstream { status: 429, .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "upstream_rate_limited",
            "evaluation provider is rate limited",
        ),
        BitrouterError::Upstream {
            status: 401 | 403, ..
        }
        | BitrouterError::UpstreamAuth { .. } => (
            StatusCode::BAD_GATEWAY,
            "upstream_authentication_failed",
            "evaluation provider authentication failed",
        ),
        BitrouterError::UpstreamInvalidResponse { .. } => (
            StatusCode::BAD_GATEWAY,
            "upstream_invalid_response",
            "evaluation provider returned an invalid response",
        ),
        BitrouterError::UpstreamTimeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "evaluation provider timed out",
        ),
        BitrouterError::Upstream {
            status: 408 | 504, ..
        } => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "evaluation provider timed out",
        ),
        BitrouterError::UpstreamUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            "evaluation provider is unavailable",
        ),
        BitrouterError::Upstream {
            status: 500..=599, ..
        } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            "evaluation provider is unavailable",
        ),
        _ => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "evaluation request failed",
        ),
    };
    let mut response = (
        status,
        Json(serde_json::json!({
            "error": { "code": code, "type": error.error_type(), "message": message }
        })),
    )
        .into_response();
    if let Some(id) = request_id {
        attach_request_id(&mut response, id);
    }
    if let BitrouterError::UpstreamRateLimited {
        retry_after: Some(seconds),
        ..
    } = error
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn model_list(config: &Config, assembled: &Assembled) -> serde_json::Value {
    let mut entries: BTreeMap<String, (Vec<String>, Vec<InferenceOperation>)> = assembled
        .routing_table
        .list_models()
        .into_iter()
        .map(|model| (model.id, (model.providers, model.operations)))
        .collect();
    if assembled.evaluation_pipeline.is_some() {
        let mut by_model: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (provider_id, provider) in &config.providers {
            if !provider.active
                || !provider
                    .operations
                    .contains_key(&InferenceOperation::Evaluate)
            {
                continue;
            }
            for model in &provider.models {
                if model.supports_operation(InferenceOperation::Evaluate) {
                    by_model.entry(&model.id).or_default().push(provider_id);
                }
            }
        }
        for (model, providers) in by_model {
            let selectors: Vec<_> = if providers.len() == 1 {
                vec![(model.to_owned(), providers[0].to_owned())]
            } else {
                providers
                    .iter()
                    .map(|provider| (format!("{provider}:{model}"), (*provider).to_owned()))
                    .collect()
            };
            for (selector, provider) in selectors {
                let entry = entries.entry(selector).or_default();
                if !entry.0.contains(&provider) {
                    entry.0.push(provider);
                }
                if !entry.1.contains(&InferenceOperation::Evaluate) {
                    entry.1.push(InferenceOperation::Evaluate);
                }
            }
        }
    }
    let data: Vec<_> = entries
        .into_iter()
        .map(|(id, (mut providers, operations))| {
            providers.sort();
            serde_json::json!({
                "id": id,
                "object": "model",
                "providers": providers,
                "operations": operations,
            })
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data })
}
