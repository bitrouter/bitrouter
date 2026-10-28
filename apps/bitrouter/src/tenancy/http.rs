//! Protected tenancy HTTP branch, separate from host inspection credentials.

use axum::{
    Extension, Json, Router,
    extract::{
        DefaultBodyLimit, Path, Request, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use bitrouter_ai::{
    control::ControlScope,
    tenancy::{
        authority::{AuditReaderCreate, BindingPut, CredentialCreate, PrincipalPut, TenantPut},
        configuration::{Activation, ConfigurationPut},
        identity::{
            BindingId, CredentialId, InstallationId, OperationId, PrincipalId, RevisionId, TenantId,
        },
        operation::{ErrorCode, Mutation},
        validation::Validate,
    },
};

use super::{Service, command::Command, error::Failure, model::Caller, store::StoredResponse};
use crate::remote_control::auth::ControlAuth;

#[derive(Clone)]
struct AuthState {
    service: Service,
    operators: ControlAuth,
}

pub(crate) async fn dispatch(
    State(service): State<Service>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" && matches!(*request.method(), Method::GET | Method::HEAD)
    {
        return next.run(request).await;
    }
    let state = match service.current() {
        Ok(state) => state,
        Err(error) => return error.into_response(),
    };
    if !service.dispatch_ready(&state) {
        return Failure::from(ErrorCode::CapabilityMismatch).into_response();
    }
    if request.method() != Method::POST {
        return Failure::from(ErrorCode::Forbidden).into_response();
    }
    let inbound = match request.uri().path() {
        "/v1/chat/completions" => bitrouter_ai::types::ApiProtocol::ChatCompletions,
        "/v1/messages" => bitrouter_ai::types::ApiProtocol::Messages,
        "/v1/responses" => bitrouter_ai::types::ApiProtocol::Responses,
        "/v1/decisions" => bitrouter_ai::types::ApiProtocol::Decisions,
        _ => return Failure::from(ErrorCode::Forbidden).into_response(),
    };
    let (parts, body) = request.into_parts();
    if one_header(&parts.headers, "origin").is_err()
        || one_header(&parts.headers, "host").is_err()
        || !crate::remote_control::origin_matches_host(&parts.headers)
        || parts.headers.contains_key(header::CONTENT_ENCODING)
    {
        return Failure::from(ErrorCode::InvalidRequest).into_response();
    }
    let submission = match service.submission(&parts.headers) {
        Ok(submission) => submission,
        Err(error) => return error.into_response(),
    };
    let bound = match one_header(&parts.headers, "content-length") {
        Ok(Some(value)) => match value.parse::<usize>() {
            Ok(value) => value,
            Err(_) => return Failure::from(ErrorCode::InvalidRequest).into_response(),
        },
        Ok(None) => 16 * 1024 * 1024,
        Err(error) => return error.into_response(),
    };
    let ingress = match service.admission.ingress(bound) {
        Ok(ingress) => ingress,
        Err(code) => return Failure::from(code).into_response(),
    };
    let body =
        match tokio::time::timeout_at(submission.deadline(), axum::body::to_bytes(body, bound))
            .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => return Failure::from(ErrorCode::PayloadTooLarge).into_response(),
            Err(_) => return Failure::from(ErrorCode::CapacityExceeded).into_response(),
        };
    let value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return Failure::from(ErrorCode::InvalidRequest).into_response(),
    };
    let input = match crate::gateway::http::parse_input(&inbound, value) {
        Ok(input) => input,
        Err(_) => return Failure::from(ErrorCode::InvalidRequest).into_response(),
    };
    drop(body);
    crate::gateway::http::handle_tenant(
        &service,
        submission,
        parts.headers,
        inbound,
        input,
        ingress,
    )
    .await
}

pub(crate) fn router(service: Service, operators: ControlAuth) -> Router {
    let router = Router::new()
        .route("/control/v1/tenancy/capabilities", get(capabilities))
        .route(
            "/control/v1/tenancy/operations/{operation_id}",
            get(operation),
        )
        .route("/control/v1/tenants/{tenant_id}", put(tenant))
        .route(
            "/control/v1/tenants/{tenant_id}/executions/{execution_id}",
            get(receipt),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/executions",
            get(receipt_page),
        )
        .route("/control/v1/principals/{principal_id}", put(principal))
        .route(
            "/control/v1/tenants/{tenant_id}/bindings/{binding_id}",
            put(binding),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/bindings/{binding_id}/management",
            delete(detach),
        )
        .route("/control/v1/tenants/{tenant_id}/credentials", post(issue))
        .route(
            "/control/v1/tenants/{tenant_id}/credentials/{credential_id}",
            get(credential).delete(revoke),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/audit-readers",
            post(audit_reader),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/configurations",
            post(put_configuration),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/configurations/{revision_id}",
            get(configuration),
        )
        .route(
            "/control/v1/tenants/{tenant_id}/activation",
            get(active_revision).post(activate),
        )
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            AuthState {
                service: service.clone(),
                operators,
            },
            authenticate,
        ))
        .with_state(service.clone());
    #[cfg(feature = "core")]
    let router = router.merge(super::eval::http::router(service));
    router
}

pub(super) fn one_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> Result<Option<&'a str>, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if values.next().is_some() {
        return Err(ErrorCode::InvalidRequest.into());
    }
    Ok(value)
}

pub(super) fn bearer(headers: &HeaderMap) -> Result<&str, Failure> {
    if headers.contains_key("x-api-key") {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let authorization = one_header(headers, "authorization")?
        .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
    let (scheme, token) = authorization
        .split_once(' ')
        .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.len() > 4096
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(ErrorCode::Unauthorized.into());
    }
    Ok(token)
}

pub(super) fn operator(operators: &ControlAuth, token: &str) -> Result<Option<Caller>, Failure> {
    if let Some(operator) = operators.authenticate(token.as_bytes()) {
        if !operator.permits(ControlScope::Tenancy) {
            return Err(ErrorCode::Forbidden.into());
        }
        return Ok(Some(Caller::Installation(super::operator_principal(
            operator.id(),
        )?)));
    }
    Ok(None)
}

fn caller(state: &AuthState, headers: &HeaderMap) -> Result<Caller, Failure> {
    let token = bearer(headers)?;
    if let Some(caller) = operator(&state.operators, token)? {
        return Ok(caller);
    }
    state.service.authenticate(token)
}

async fn authenticate(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorized = (|| {
        one_header(request.headers(), "origin")?;
        one_header(request.headers(), "host")?;
        if !crate::remote_control::origin_matches_host(request.headers()) {
            return Err(Failure::from(ErrorCode::Forbidden));
        }
        if request.headers().contains_key(header::CONTENT_ENCODING) {
            return Err(Failure::from(ErrorCode::InvalidRequest));
        }
        let caller = caller(&state, request.headers())?;
        let installation = one_header(request.headers(), "bitrouter-installation-id")?;
        if request.method() == Method::GET
            && request.uri().path() != "/control/v1/tenancy/capabilities"
            && installation.is_none()
        {
            return Err(Failure::from(ErrorCode::InvalidRequest));
        }
        if let Some(installation) = installation {
            let installation: InstallationId = installation
                .try_into()
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            state.service.check_installation(&installation)?;
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
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

fn body<T: Validate>(input: Result<Json<T>, JsonRejection>) -> Result<T, Failure> {
    let Json(value) = input.map_err(|error| {
        Failure::from(if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ErrorCode::PayloadTooLarge
        } else {
            ErrorCode::InvalidRequest
        })
    })?;
    value
        .validate()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    Ok(value)
}

fn path<T>(input: Result<Path<T>, PathRejection>) -> Result<T, Failure> {
    input
        .map(|Path(value)| value)
        .map_err(|_| ErrorCode::InvalidRequest.into())
}

fn response(value: StoredResponse) -> Response {
    match value {
        StoredResponse::Operation(result) => Json(result).into_response(),
        StoredResponse::Issued(issued) => Json(issued).into_response(),
        StoredResponse::Configuration(configuration) => Json(configuration).into_response(),
        StoredResponse::Failure(error) => Failure(error).into_response(),
    }
}

async fn capabilities(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, Failure> {
    Ok(Json(service.capabilities(&caller)?).into_response())
}

async fn operation(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<OperationId>, PathRejection>,
) -> Result<Response, Failure> {
    Ok(Json(service.operation(&caller, &path(input)?).await?).into_response())
}

async fn credential(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, CredentialId)>, PathRejection>,
) -> Result<Response, Failure> {
    let (tenant, id) = path(input)?;
    Ok(Json(service.credential(&caller, &tenant, &id).await?).into_response())
}

async fn tenant(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<TenantId>, PathRejection>,
    input: Result<Json<TenantPut>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::Tenant(path(id)?, body(input)?))
            .await?,
    ))
}

async fn principal(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<PrincipalId>, PathRejection>,
    input: Result<Json<PrincipalPut>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::Principal(path(id)?, body(input)?))
            .await?,
    ))
}

async fn binding(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    ids: Result<Path<(TenantId, BindingId)>, PathRejection>,
    input: Result<Json<BindingPut>, JsonRejection>,
) -> Result<Response, Failure> {
    let (tenant, id) = path(ids)?;
    Ok(response(
        service
            .execute(caller, Command::Binding(tenant, id, body(input)?))
            .await?,
    ))
}

async fn issue(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<TenantId>, PathRejection>,
    input: Result<Json<CredentialCreate>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::Issue(path(id)?, body(input)?))
            .await?,
    ))
}

async fn audit_reader(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    id: Result<Path<TenantId>, PathRejection>,
    input: Result<Json<AuditReaderCreate>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::AuditReader(path(id)?, body(input)?))
            .await?,
    ))
}

async fn detach(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    ids: Result<Path<(TenantId, BindingId)>, PathRejection>,
    input: Result<Json<Mutation>, JsonRejection>,
) -> Result<Response, Failure> {
    let (tenant, id) = path(ids)?;
    Ok(response(
        service
            .execute(caller, Command::Detach(tenant, id, body(input)?))
            .await?,
    ))
}

async fn revoke(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    ids: Result<Path<(TenantId, CredentialId)>, PathRejection>,
    input: Result<Json<Mutation>, JsonRejection>,
) -> Result<Response, Failure> {
    let (tenant, id) = path(ids)?;
    Ok(response(
        service
            .execute(caller, Command::Revoke(tenant, id, body(input)?))
            .await?,
    ))
}

async fn put_configuration(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    tenant: Result<Path<TenantId>, PathRejection>,
    input: Result<Json<ConfigurationPut>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::Configuration(path(tenant)?, body(input)?))
            .await?,
    ))
}

async fn activate(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    tenant: Result<Path<TenantId>, PathRejection>,
    input: Result<Json<Activation>, JsonRejection>,
) -> Result<Response, Failure> {
    Ok(response(
        service
            .execute(caller, Command::Activate(path(tenant)?, body(input)?))
            .await?,
    ))
}

async fn configuration(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    target: Result<Path<(TenantId, RevisionId)>, PathRejection>,
) -> Result<Response, Failure> {
    let (tenant, revision) = path(target)?;
    Ok(Json(service.configuration(&caller, &tenant, &revision).await?).into_response())
}

async fn active_revision(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    tenant: Result<Path<TenantId>, PathRejection>,
) -> Result<Response, Failure> {
    Ok(Json(service.active_revision(&caller, &path(tenant)?)?).into_response())
}

async fn receipt(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, bitrouter_ai::tenancy::identity::ExecutionId)>, PathRejection>,
) -> Result<Response, Failure> {
    let (tenant, id) = path(input)?;
    let body = service
        .read_receipt(caller, tenant, super::receipts::read::Request::One(id))
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptQuery {
    correlation_id: bitrouter_ai::tenancy::identity::CorrelationId,
    after: Option<bitrouter_ai::tenancy::identity::ExecutionId>,
    limit: Option<u64>,
}
async fn receipt_page(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<TenantId>, PathRejection>,
    query: Result<axum::extract::Query<ReceiptQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, Failure> {
    let tenant = path(input)?;
    let axum::extract::Query(query) =
        query.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let body = service
        .read_receipt(
            caller,
            tenant,
            super::receipts::read::Request::Page {
                correlation: query.correlation_id,
                after: query.after,
                limit,
            },
        )
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}
