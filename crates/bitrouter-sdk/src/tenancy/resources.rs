//! Protected resource provisioning, separate from ingress credential issuance.

use super::{TenancyClient, transport};
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{InstallationId, TenantId},
    operation::OperationResult,
    resources::{ResourceCapabilities, ResourcePut, ResourceRevoke, ResourceStatus},
};
use reqwest::Method;

impl TenancyClient {
    /// Discover the independently versioned dynamic resource contract.
    ///
    /// # Errors
    /// Returns authentication, installation, transport or contract errors.
    pub async fn resource_capabilities(
        &self,
        installation: &InstallationId,
    ) -> Result<ResourceCapabilities> {
        let result: ResourceCapabilities = self
            .get("tenancy/resource-capabilities", Some(installation))
            .await?;
        if &result.installation_id != installation {
            return Err(Error::Response("resource capability installation mismatch"));
        }
        Ok(result)
    }

    /// Provision a new encrypted immutable revision with explicit managed-head CAS.
    ///
    /// # Errors
    /// Returns validation, authority/CAS errors or an unknown operation outcome.
    /// No mutation is retried automatically. Recover using the original operation ID.
    pub async fn put_resource(
        &self,
        tenant: &TenantId,
        input: &ResourcePut,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/resources"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(
            result,
            &input.operation_id,
            Some(input.key.resource_id.as_str()),
            None,
        )
    }

    /// Revoke an exact resource revision while retaining historical evidence.
    ///
    /// # Errors
    /// Returns validation, authority/CAS errors or an unknown operation outcome.
    pub async fn revoke_resource(
        &self,
        tenant: &TenantId,
        input: &ResourceRevoke,
    ) -> Result<OperationResult> {
        let result = self
            .mutate(
                Method::POST,
                &format!("tenants/{tenant}/resources/revoke"),
                &input.installation_id,
                &input.operation_id,
                input,
            )
            .await?;
        transport::operation_result(
            result,
            &input.operation_id,
            Some(input.reference.resource_id.as_str()),
            Some(&input.reference.revision_id),
        )
    }

    /// Read nonsecret status; definitions and provider secrets are never returned.
    ///
    /// # Errors
    /// Returns authorization, transport, missing-resource or identity errors.
    pub async fn resource_status(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        reference: &ResourceRef,
    ) -> Result<ResourceStatus> {
        let request = self
            .http
            .get(self.url(&format!("tenants/{tenant}/resources"))?)
            .header("Bitrouter-Installation-Id", installation.as_str())
            .query(reference);
        let status: ResourceStatus = self.send(request).await?;
        if &status.installation_id != installation || &status.reference != reference {
            return Err(Error::Response("resource status identity mismatch"));
        }
        Ok(status)
    }
}
