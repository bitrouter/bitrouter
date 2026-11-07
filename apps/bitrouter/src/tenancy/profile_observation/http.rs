//! A separate bounded authentication path avoids blocking legacy read locks.

use axum::{
    Router,
    extract::{Request, State},
    http::header,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bitrouter_ai::tenancy::{
    identity::InstallationId,
    operation::ErrorCode,
    profile_observation::{AdmissionProfileObservationRequestV1, MAX_BYTES},
};
use tokio::time::Instant;

use crate::{
    remote_control::auth::ControlAuth,
    tenancy::{
        MANAGEMENT_TIMEOUT, Service,
        error::Failure,
        http::{bearer, one_header, operator},
    },
};

#[derive(Clone)]
struct Auth {
    service: Service,
    operators: ControlAuth,
}

pub(in crate::tenancy) fn router(service: Service, operators: ControlAuth) -> Router {
    Router::new()
        .route(
            "/control/v1/tenancy/admission-profiles/observations/capabilities",
            get(capabilities),
        )
        .route(
            "/control/v1/tenancy/admission-profiles/observations/v1",
            post(observe),
        )
        .with_state(Auth { service, operators })
}

async fn capabilities(State(auth): State<Auth>, request: Request) -> Response {
    respond(&auth, request, false).await
}

async fn observe(State(auth): State<Auth>, request: Request) -> Response {
    respond(&auth, request, true).await
}

async fn respond(auth: &Auth, request: Request, query: bool) -> Response {
    let at = Instant::now() + MANAGEMENT_TIMEOUT;
    let result = read(auth, request, query, at).await;
    let mut response = match result {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

async fn read(auth: &Auth, request: Request, query: bool, at: Instant) -> Result<Vec<u8>, Failure> {
    super::deadline(at)?;
    let (parts, body) = request.into_parts();
    one_header(&parts.headers, "origin")?;
    one_header(&parts.headers, "host")?;
    if !crate::remote_control::origin_matches_host(&parts.headers) {
        return Err(ErrorCode::Forbidden.into());
    }
    if parts.uri.query().is_some() || parts.headers.contains_key(header::CONTENT_ENCODING) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let caller = operator(&auth.operators, bearer(&parts.headers)?)?
        .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
    let installation = one_header(&parts.headers, "bitrouter-installation-id")?
        .map(InstallationId::try_from)
        .transpose()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if query && installation.is_none() {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let read = auth
        .service
        .profile_observation_read(&caller, installation.as_ref(), at)?;
    if !query {
        return read.capabilities();
    }
    let content_type = one_header(&parts.headers, "content-type")?
        .ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?;
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(ErrorCode::InvalidRequest.into());
    }
    if let Some(length) = one_header(&parts.headers, "content-length")? {
        let length: usize = length
            .parse()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        if length > MAX_BYTES {
            return Err(ErrorCode::PayloadTooLarge.into());
        }
    }
    let bytes = match tokio::time::timeout_at(at, axum::body::to_bytes(body, MAX_BYTES)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return Err(ErrorCode::PayloadTooLarge.into()),
        Err(_) => return Err(ErrorCode::CapacityExceeded.into()),
    };
    super::deadline(at)?;
    let request: AdmissionProfileObservationRequestV1 =
        serde_json::from_slice(&bytes).map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    drop(bytes);
    let snapshot = read.sample(&request)?;
    read.finish(&snapshot)
}
