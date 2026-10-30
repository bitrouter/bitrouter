//! Additive ledger transport on the existing protected control listener.
use super::{Action, BODY_BYTES, Job, WORK, wire};
use crate::tenancy::{Service, error::Failure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bitrouter_ai::tenancy::{identity::TenantId, operation::ErrorCode};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

type TenantPath = Result<Path<String>, PathRejection>;
type ObjectPath = Result<Path<(String, String)>, PathRejection>;

pub(in crate::tenancy) fn router(service: Service) -> Router {
    Router::new()
        .route(
            "/control/v1/tenants/{tenant_id}/evals/subjects",
            post(put).get(subjects),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/evals/subjects/{eval_id}",
            get(subject),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/evals/results",
            post(submit),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/evals/snapshots",
            post(freeze),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/evals/snapshots/{evidence_root}",
            get(snapshot),
        )
        .route("/control/v1/tenants/{tenant_id}/evals/status", get(status))
        .layer(axum::middleware::from_fn(no_store))
        .with_state(service)
}
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn put(State(service): State<Service>, path: TenantPath, request: Request) -> Response {
    tenant(service, path, Endpoint::Put, request).await
}
async fn subjects(State(service): State<Service>, path: TenantPath, request: Request) -> Response {
    tenant(service, path, Endpoint::Subjects, request).await
}
async fn submit(State(service): State<Service>, path: TenantPath, request: Request) -> Response {
    tenant(service, path, Endpoint::Submit, request).await
}
async fn freeze(State(service): State<Service>, path: TenantPath, request: Request) -> Response {
    tenant(service, path, Endpoint::Freeze, request).await
}
async fn status(State(service): State<Service>, path: TenantPath, request: Request) -> Response {
    tenant(service, path, Endpoint::Status, request).await
}
async fn subject(State(service): State<Service>, path: ObjectPath, request: Request) -> Response {
    let Ok(Path((tenant, id))) = path else {
        return Failure::from(ErrorCode::InvalidRequest).into_response();
    };
    handle(service, tenant, Endpoint::Subject(id), request)
        .await
        .into_response()
}
async fn snapshot(State(service): State<Service>, path: ObjectPath, request: Request) -> Response {
    let Ok(Path((tenant, id))) = path else {
        return Failure::from(ErrorCode::InvalidRequest).into_response();
    };
    handle(service, tenant, Endpoint::Snapshot(id), request)
        .await
        .into_response()
}
async fn tenant(
    service: Service,
    path: TenantPath,
    endpoint: Endpoint,
    request: Request,
) -> Response {
    let Ok(Path(tenant)) = path else {
        return Failure::from(ErrorCode::InvalidRequest).into_response();
    };
    handle(service, tenant, endpoint, request)
        .await
        .into_response()
}

enum Endpoint {
    Put,
    Subjects,
    Subject(String),
    Submit,
    Freeze,
    Snapshot(String),
    Status,
}
impl Endpoint {
    fn action(&self) -> Action {
        match self {
            Self::Put | Self::Submit => Action::Submit,
            Self::Freeze => Action::Freeze,
            _ => Action::Read,
        }
    }
    fn parse(self, body: &[u8]) -> Result<super::Request, Failure> {
        Ok(match self {
            Self::Put => super::Request::Put(wire::parse(body)?),
            Self::Submit => super::Request::Submit(wire::parse(body)?),
            Self::Freeze => super::Request::Freeze(wire::freeze(body)?),
            _ if !body.is_empty() => return Err(ErrorCode::InvalidRequest.into()),
            Self::Subjects => super::Request::Subjects,
            Self::Subject(id) => super::Request::Subject(id),
            Self::Snapshot(id) => super::Request::Snapshot(id),
            Self::Status => super::Request::Status,
        })
    }
}

async fn handle(
    service: Service,
    tenant: String,
    endpoint: Endpoint,
    request: Request,
) -> Result<Response, Failure> {
    let deadline = Instant::now() + WORK;
    if request.uri().query().is_some() {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let tenant: TenantId = tenant
        .try_into()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if let Endpoint::Subject(id) | Endpoint::Snapshot(id) = &endpoint
        && (id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control))
    {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let action = endpoint.action();
    let identity = service.eval_identity(request.headers(), tenant, action)?;
    let lease = service.eval_lease()?;
    let one = crate::tenancy::http::one_header;
    let content_type = one(request.headers(), "content-type")?;
    let body_limit = if matches!(endpoint, Endpoint::Freeze) {
        4 * 1024
    } else {
        BODY_BYTES
    };
    if let Some(length) = one(request.headers(), "content-length")? {
        let length: usize = length
            .parse()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        if length > body_limit {
            return Err(ErrorCode::PayloadTooLarge.into());
        }
    }
    let json = content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    });
    let body = tokio::time::timeout_at(
        deadline,
        axum::body::to_bytes(request.into_body(), body_limit),
    )
    .await
    .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
    .map_err(|_| Failure::from(ErrorCode::PayloadTooLarge))?;
    if (matches!(endpoint, Endpoint::Put | Endpoint::Submit)
        || matches!(endpoint, Endpoint::Freeze) && !body.is_empty())
        && !json
    {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let request = endpoint.parse(&body)?;
    drop(body);
    if Instant::now() >= deadline {
        return Err(ErrorCode::CapacityExceeded.into());
    }
    let (reply, receive) = tokio::sync::oneshot::channel();
    service.enqueue_eval(Job {
        identity: identity.clone(),
        request,
        deadline,
        lease,
        reply,
    })?;
    let reply = tokio::time::timeout_at(deadline, receive)
        .await
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))??;
    #[cfg(test)]
    {
        let gate = service
            .eval
            .handoff
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }
    if Instant::now() >= deadline {
        return Err(ErrorCode::AuthorityUnavailable.into());
    }
    service.check_eval(&identity, action)?;
    // No await after current authority is checked. The capacity lease follows
    // the actual response body until transport completion or cancellation.
    let mut response = Response::new(Body::new(TrackedBody {
        inner: Box::pin(Body::from(reply.body)),
        _lease: reply.lease,
    }));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(response)
}
struct TrackedBody {
    inner: Pin<Box<Body>>,
    _lease: OwnedSemaphorePermit,
}
impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        self.get_mut().inner.as_mut().poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
