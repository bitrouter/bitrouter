//! Read-only provider observations through the existing bounded Core transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    provider_technical_facts::{MAX_BYTES, ProviderTechnicalFactsV1},
};

impl TenancyClient {
    /// Read optional provider measurements for the original execution. Null means
    /// not retained, never zero or a reason to repeat inference. Old Core routing
    /// or version errors are returned through the existing service-error surface.
    ///
    /// # Errors
    /// Returns authorization, unsupported/missing execution, bounded transport,
    /// schema or identity errors. Pair with validate_against[_v2] for a receipt.
    pub async fn provider_technical_facts_v1(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<Option<ProviderTechnicalFactsV1>> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{tenant}/executions/{execution}/provider-technical-facts/v1"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        let result: Option<ProviderTechnicalFactsV1> =
            self.send_bounded(request, MAX_BYTES).await?;
        if result.as_ref().is_some_and(|facts| {
            &facts.installation_id != installation
                || &facts.tenant_id != tenant
                || &facts.execution_id != execution
        }) {
            return Err(Error::Response("provider technical identity mismatch"));
        }
        Ok(result)
    }
}
