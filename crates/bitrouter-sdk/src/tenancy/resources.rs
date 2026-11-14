//! Protected resource provisioning, separate from ingress credential issuance.

use super::{TenancyClient, transport};
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{InstallationId, TenantId},
    operation::OperationResult,
    resources::{ResourceCapabilities, ResourcePut, ResourceRevoke, ResourceStatus},
    resources_v2::{ResourceCapabilitiesV2, ResourcePutV2},
    resources_v3::{ResourceCapabilitiesV3, ResourcePutV3},
};
use reqwest::Method;

impl TenancyClient {
    /// Discover explicit native-route provisioning without widening v1 or v2.
    ///
    /// # Errors
    /// Returns validation, authentication, installation or transport errors.
    pub async fn resource_capabilities_v3(
        &self,
        installation: &InstallationId,
    ) -> Result<ResourceCapabilitiesV3> {
        let result: ResourceCapabilitiesV3 = self
            .get("tenancy/resource-capabilities/v3", Some(installation))
            .await?;
        if &result.installation_id != installation {
            return Err(Error::Response("resource capability installation mismatch"));
        }
        Ok(result)
    }

    /// Provision an explicit native route using the existing managed-head CAS.
    ///
    /// # Errors
    /// Returns validation, authorization, CAS or uncertain-operation errors.
    /// Never retries or downgrades the native attachments to a legacy format.
    pub async fn put_resource_v3(
        &self,
        tenant: &TenantId,
        input: &ResourcePutV3,
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

    /// Discover explicit shared-health profile provisioning without changing v1 discovery.
    ///
    /// # Errors
    /// Returns authentication, unsupported contract, installation or transport errors.
    pub async fn resource_capabilities_v2(
        &self,
        installation: &InstallationId,
    ) -> Result<ResourceCapabilitiesV2> {
        let result: ResourceCapabilitiesV2 = self
            .get("tenancy/resource-capabilities/v2", Some(installation))
            .await?;
        if &result.installation_id != installation {
            return Err(Error::Response("resource capability installation mismatch"));
        }
        Ok(result)
    }

    /// Provision a shared-health member with explicit managed-head CAS.
    ///
    /// # Errors
    /// Returns validation, authorization, CAS or uncertain-operation errors.
    /// Never retries as v1 or removes an unsupported health-group binding.
    pub async fn put_resource_v2(
        &self,
        tenant: &TenantId,
        input: &ResourcePutV2,
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
