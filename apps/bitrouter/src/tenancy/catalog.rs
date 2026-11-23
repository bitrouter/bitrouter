//! Authorized reads of the immutable process-configured catalog.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use bitrouter_ai::tenancy::{
    authority::Permission,
    catalog::{CatalogCandidatesV1, CatalogSelectionRequestV1, CatalogSelectionV1},
    identity::TenantId,
    operation::ErrorCode,
    ordered::GenerationProtocol,
    validation::Validate,
};
use serde::Deserialize;

use super::{Service, actor::AuthoritySubject, error::Failure, model::Caller};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CandidatesQuery {
    inbound_protocol: GenerationProtocol,
}

pub(super) async fn candidates(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    Path((tenant, route)): Path<(TenantId, String)>,
    Query(query): Query<CandidatesQuery>,
) -> Result<Json<CatalogCandidatesV1>, Failure> {
    service
        .catalog_candidates(&caller, &tenant, &route, query.inbound_protocol)
        .await
        .map(Json)
}

pub(super) async fn prepare(
    State(service): State<Service>,
    Extension(caller): Extension<Caller>,
    Path(tenant): Path<TenantId>,
    Json(input): Json<CatalogSelectionRequestV1>,
) -> Result<Json<CatalogSelectionV1>, Failure> {
    service
        .prepare_catalog_selection(&caller, &tenant, &input)
        .await
        .map(Json)
}

impl Service {
    pub(in crate::tenancy) async fn prepare_catalog_selection(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        input: &CatalogSelectionRequestV1,
    ) -> Result<CatalogSelectionV1, Failure> {
        input
            .validate()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        if input.candidates.len() > usize::from(self.execution_settings.ordered_attempt_limit) {
            return Err(ErrorCode::CapacityExceeded.into());
        }
        let catalog = self.resources.configured().ok_or(ErrorCode::NotFound)?;
        let root = catalog
            .root_reference(input.route_id.as_str())
            .ok_or(ErrorCode::NotFound)?;
        let cheap: Vec<_> = std::iter::once(root)
            .chain(input.candidates.iter().flat_map(|candidate| {
                candidate
                    .credential_ref
                    .iter()
                    .chain(candidate.route_ref.iter())
            }))
            .cloned()
            .collect();
        let authenticated = self
            .authority_read(AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: None,
            })
            .await?;
        let revision = self
            .authority
            .read()
            .map_err(Failure::from)?
            .check(&authenticated.lease, |view| {
                super::configuration::authorize(
                    view.facts(),
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    &cheap,
                    chrono::Utc::now(),
                )?;
                Ok(view.facts().revision)
            })
            .map_err(Failure::from)?;
        let (execution, mut result) = {
            let _read = self.read_permit()?;
            let owned = tokio::time::timeout(
                super::MANAGEMENT_TIMEOUT,
                catalog.owned_targets(&self.resources, &self.db, tenant, input),
            )
            .await
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            catalog
                .prepare_with_targets(&self.resources, tenant, input, revision, &owned)
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?
        };
        let route = execution
            .catalog_dependencies(input.route_id.as_str())
            .map_err(Failure::from)?;
        // These are descriptor reads, not an execution dependency owner. Refresh
        // the real tenant and caller after expansion; all references are authorized
        // against that same current view before any descriptor leaves the service.
        drop(authenticated);
        let authenticated = self
            .authority_read(AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: None,
            })
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&authenticated.lease, |view| {
                super::configuration::authorize(
                    view.facts(),
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    &route,
                    chrono::Utc::now(),
                )?;
                result.candidates.manifest.authority_revision = view.facts().revision;
                Ok(())
            })
            .map_err(Failure::from)?;
        Ok(result)
    }

    pub(in crate::tenancy) async fn catalog_candidates(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        route: &str,
        protocol: GenerationProtocol,
    ) -> Result<CatalogCandidatesV1, Failure> {
        let catalog = self.resources.configured().ok_or(ErrorCode::NotFound)?;
        let root = catalog.root_reference(route).ok_or(ErrorCode::NotFound)?;
        let authenticated = self
            .authority_read(AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: None,
            })
            .await?;
        let revision = self
            .authority
            .read()
            .map_err(Failure::from)?
            .check(&authenticated.lease, |view| {
                super::configuration::authorize(
                    view.facts(),
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    std::slice::from_ref(root),
                    chrono::Utc::now(),
                )?;
                Ok(view.facts().revision)
            })
            .map_err(Failure::from)?;
        let mut result = {
            let _read = self.read_permit()?;
            catalog
                .candidates(tenant, route, protocol, revision)
                .map_err(|_| Failure::from(ErrorCode::NotFound))?
        };
        let mut references = vec![result.manifest.route.clone()];
        for candidate in &result.manifest.candidates {
            for reference in
                std::iter::once(&candidate.credential).chain(candidate.admission_profile.iter())
            {
                if !references.contains(reference) {
                    references.push(reference.clone());
                }
            }
        }
        // These are descriptor reads, not an execution dependency owner. Refresh
        // the real tenant and caller after expansion; all references are authorized
        // against that same current view before any descriptor leaves the service.
        drop(authenticated);
        let authenticated = self
            .authority_read(AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: None,
            })
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&authenticated.lease, |view| {
                super::configuration::authorize(
                    view.facts(),
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    &references,
                    chrono::Utc::now(),
                )?;
                result.manifest.authority_revision = view.facts().revision;
                Ok(())
            })
            .map_err(Failure::from)?;
        result.validate().map_err(|_| self.unavailable())?;
        Ok(result)
    }
}
