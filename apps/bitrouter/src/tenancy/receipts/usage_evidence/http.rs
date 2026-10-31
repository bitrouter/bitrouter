//! Protected handlers mounted inside the existing tenancy authorization branch.
use crate::tenancy::{Service, error::Failure, model::Caller};
use axum::{
    Extension, Json,
    extract::{Path, State, rejection::PathRejection},
    http::header,
    response::{IntoResponse, Response},
};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, TenantId},
    operation::ErrorCode,
    usage_evidence::UsageEvidenceCapabilitiesV1,
};

pub(in crate::tenancy) async fn capabilities(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, Failure> {
    let installed = service.capabilities(&caller)?;
    let mut capabilities = UsageEvidenceCapabilitiesV1::new(installed.installation_id);
    capabilities.child_linkage = cfg!(all(feature = "core", feature = "harness"));
    Ok(Json(capabilities).into_response())
}

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
            super::super::read::Request::UsageEvidence(id),
        )
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}
