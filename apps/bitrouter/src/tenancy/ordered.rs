//! Explicit ordered-mode discovery and operator-only candidate descriptors.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{ExecutionId, OwnerId, ResourceId, RevisionId, TenantId},
    operation::ErrorCode,
    ordered::{CandidateManifestV1, GenerationProtocol, OrderedCapabilitiesV2},
};
use serde::Deserialize;

use super::{Service, configuration_store, error::Failure, model::Caller};

pub(super) async fn capabilities(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<OrderedCapabilitiesV2>, Failure> {
    service
        .with_capability_authority(&caller, |state| {
            let enabled = service.ordered_dispatch_ready(state);
            Ok(Json(OrderedCapabilitiesV2 {
                contract_version: 2,
                installation_id: state.installation.clone(),
                instance_id: service.instance.clone(),
                capability_revision: service.capability_revision(state)?,
                authority_revision: state.revision,
                attempt_limit: service.execution_settings.ordered_attempt_limit,
                revision_bound_dispatch: enabled,
                strict_receipts: enabled,
                dispatch_enabled: enabled,
            }))
        })
        .await
}

pub(super) async fn receipt(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, ExecutionId)>, axum::extract::rejection::PathRejection>,
) -> Result<Response, Failure> {
    let Path((tenant, id)) = input.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let body = service
        .read_receipt(caller, tenant, super::receipts::read::Request::OneV2(id))
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

pub(super) async fn evidence(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, ExecutionId)>, axum::extract::rejection::PathRejection>,
) -> Result<Response, Failure> {
    let Path((tenant, id)) = input.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let body = service
        .read_receipt(
            caller,
            tenant,
            super::receipts::read::Request::UsageEvidenceV2(id),
        )
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestQuery {
    route_owner_kind: OwnerKind,
    route_owner_id: OwnerId,
    route_id: ResourceId,
    route_revision: RevisionId,
    inbound_protocol: GenerationProtocol,
}

pub(super) async fn manifest(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    input: Result<Path<(TenantId, RevisionId)>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ManifestQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<CandidateManifestV1>, Failure> {
    if !matches!(caller, Caller::Installation(_)) {
        return Err(ErrorCode::Forbidden.into());
    }
    let Path((tenant, revision)) = input.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let Query(query) = query.map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    let reference = ResourceRef {
        owner_kind: query.route_owner_kind,
        owner_id: query.route_owner_id,
        resource_kind: ResourceKind::Route,
        resource_id: query.route_id,
        revision_id: query.route_revision,
    };
    service
        .candidate_manifest(
            &caller,
            &tenant,
            &revision,
            &reference,
            query.inbound_protocol,
        )
        .await
        .map(Json)
}

impl Service {
    pub(super) async fn candidate_manifest(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        revision: &RevisionId,
        reference: &ResourceRef,
        inbound: GenerationProtocol,
    ) -> Result<CandidateManifestV1, Failure> {
        if !matches!(caller, Caller::Installation(_)) {
            return Err(ErrorCode::Forbidden.into());
        }
        let _read = self.read_permit()?;
        let before = self.current()?;
        before
            .principal(caller, chrono::Utc::now())
            .map_err(Failure::from)?;
        let stored = tokio::time::timeout(
            super::MANAGEMENT_TIMEOUT,
            configuration_store::configuration(&self.db, tenant, revision),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(|_| self.unavailable())?
        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        let view = self
            .resources
            .open_execution(&stored)
            .map_err(|_| self.unavailable())?;
        let manifest = view
            .candidate_manifest(reference, inbound, before.revision)
            .map_err(Failure::from)?;
        let after = self.current()?;
        after
            .principal(caller, chrono::Utc::now())
            .map_err(Failure::from)?;
        if after.revision != before.revision || stored.authority_revision > after.revision {
            return Err(ErrorCode::RevisionConflict.into());
        }
        Ok(manifest)
    }
}
