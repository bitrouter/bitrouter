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
    /// Discover the exact policy contract and serving classification digest.
    ///
    /// # Errors
    /// Returns authority, unsupported-contract, transport or installation errors.
    pub async fn policy_capabilities(
        &self,
        installation: &InstallationId,
    ) -> Result<bitrouter_ai::tenancy::resources::PolicyCapabilities> {
        let capabilities: bitrouter_ai::tenancy::resources::PolicyCapabilities = self
            .get("tenancy/policy-capabilities", Some(installation))
            .await?;
        if &capabilities.installation_id != installation {
            return Err(Error::Response("policy capability installation mismatch"));
        }
        Ok(capabilities)
    }

    /// Read model-only policy source with exact resource-read authority.
    ///
    /// # Errors
    /// Returns authorization, unsupported-policy, transport or identity errors.
    pub async fn policy_source(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        reference: &ResourceRef,
    ) -> Result<bitrouter_ai::tenancy::resources::PolicySource> {
        let request = self
            .http
            .get(self.url(&format!("tenants/{tenant}/resources/policy"))?)
            .header("Bitrouter-Installation-Id", installation.as_str())
            .query(reference);
        let source: bitrouter_ai::tenancy::resources::PolicySource = self.send(request).await?;
        if &source.status.installation_id != installation || &source.status.reference != reference {
            return Err(Error::Response("policy source identity mismatch"));
        }
        Ok(source)
    }

    /// Discover current resource provisioning capabilities.
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
