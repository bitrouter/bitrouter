use super::{Archive, Reply};
use crate::{
    remote_control::auth::ControlAuth,
    tenancy::{error::Failure, model::Caller, receipts::read},
};
use axum::{
    Extension, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use bitrouter_ai::tenancy::{
    identity::{CorrelationId, ExecutionId, InstallationId, TenantId},
    operation::ErrorCode,
};

#[derive(Clone)]
struct Auth {
    archive: Archive,
    operators: ControlAuth,
}
pub(super) fn router(archive: Archive, operators: ControlAuth) -> Router {
    Router::new()
        .route(
            "/control/v1/tenants/{tenant_id}/executions/{execution_id}",
            get(one),
        )
        .route("/control/v1/tenants/{tenant_id}/executions", get(page))
        .fallback(|| async { Failure::from(ErrorCode::NotFound) })
        .layer(axum::middleware::from_fn_with_state(
            Auth {
                archive: archive.clone(),
                operators,
            },
            authenticate,
        ))
        .with_state(archive)
}
async fn authenticate(State(state): State<Auth>, mut request: Request, next: Next) -> Response {
    let authorized = (|| {
        use crate::tenancy::http::{bearer, one_header, operator};
        let headers = request.headers();
        if request.uri().to_string().len() > 2048
            || headers.len() > 32
            || headers
                .iter()
                .map(|(key, value)| key.as_str().len() + value.len())
                .sum::<usize>()
                > 16 * 1024
        {
            return Err(Failure::from(ErrorCode::InvalidRequest));
        }
        one_header(headers, "origin")?;
        one_header(headers, "host")?;
        if !crate::remote_control::origin_matches_host(headers) {
            return Err(ErrorCode::Forbidden.into());
        }
        if headers.contains_key(header::CONTENT_ENCODING)
            || headers.contains_key(header::TRANSFER_ENCODING)
            || one_header(headers, "content-length")?.is_some_and(|n| n != "0")
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let caller = operator(&state.operators, bearer(headers)?)?
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        let installation: InstallationId = one_header(headers, "bitrouter-installation-id")?
            .ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?
            .try_into()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        if installation != state.archive.state.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        if state.archive.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        // Axum GET normally supplies HEAD; this profile exposes exact GET only.
        if request.method() == Method::HEAD {
            return Err(ErrorCode::InvalidRequest.into());
        }
        Ok::<_, Failure>(caller)
    })();
    let mut response = match authorized {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn response(reply: Reply) -> Response {
    // Bytes owns the permit, including transport-created clones/slices after
    // the response body is polled or dropped. A slow peer cannot free a slot
    // while its encoded reply allocation is still retained by the transport.
    (
        [(header::CONTENT_TYPE, "application/json")],
        axum::body::Bytes::from_owner(reply),
    )
        .into_response()
}
async fn one(
    State(archive): State<Archive>,
    Extension(caller): Extension<Caller>,
    path: Result<Path<(TenantId, ExecutionId)>, axum::extract::rejection::PathRejection>,
) -> Result<Response, Failure> {
    let Path((tenant, id)) = path.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    Ok(response(
        archive.read(caller, tenant, read::Request::One(id)).await?,
    ))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    correlation_id: CorrelationId,
    after: Option<ExecutionId>,
    limit: Option<u64>,
}
async fn page(
    State(archive): State<Archive>,
    Extension(caller): Extension<Caller>,
    path: Result<Path<TenantId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Failure> {
    let Path(tenant) = path.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let Query(query) = query.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let limit = query.limit.map_or(100, |value| value);
    if !(1..=100).contains(&limit) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    Ok(response(
        archive
            .read(
                caller,
                tenant,
                read::Request::Page {
                    correlation: query.correlation_id,
                    after: query.after,
                    limit,
                },
            )
            .await?,
    ))
}
