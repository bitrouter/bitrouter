//! Protected read; existing ReceiptRead authorization and read custody apply.
use crate::tenancy::{Service, error::Failure, model::Caller};
use axum::{
    Extension,
    extract::{Path, State, rejection::PathRejection},
    http::header,
    response::{IntoResponse, Response},
};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, TenantId},
    operation::ErrorCode,
};

pub(in crate::tenancy) async fn read(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, ExecutionId)>, PathRejection>,
) -> Result<Response, Failure> {
    let Path((tenant, id)) = input.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let body = service
        .read_receipt(
            caller,
            tenant,
            super::super::read::Request::ProviderTechnical(id),
        )
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

pub(in crate::tenancy) async fn read_v2(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, ExecutionId)>, PathRejection>,
) -> Result<Response, Failure> {
    let Path((tenant, id)) = input.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let body = service
        .read_receipt(
            caller,
            tenant,
            super::super::read::Request::ProviderTechnicalV2(id),
        )
        .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}
