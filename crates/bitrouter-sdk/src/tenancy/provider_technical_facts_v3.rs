//! Read actual Core-owned scope through the existing bounded Core transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    provider_technical_facts::MAX_BYTES,
    provider_technical_facts_v3::ProviderTechnicalFactsV3,
};

impl TenancyClient {
    /// Read optional actual observer callbacks and the unchanged scope. Null also
    /// covers old retained payload1/2 without payload3 observations; no backfill or
    /// original inference is attempted. Validate against the original receipt
    /// before interpreting any actual-arm or parent/child association.
    ///
    /// # Errors
    /// Original authorization, unsupported Core route, bounded transfer,
    /// malformed schema or identity errors. This method never retries or sends
    /// a provider request, resumes a stream, or falls back to another source.
    pub async fn provider_technical_facts_v3(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<Option<ProviderTechnicalFactsV3>> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{tenant}/executions/{execution}/provider-technical-facts/v3"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        let result: Option<ProviderTechnicalFactsV3> =
            self.send_bounded(request, MAX_BYTES).await?;
        if result.as_ref().is_some_and(|facts| {
            &facts.scope.installation_id != installation
                || &facts.scope.tenant_id != tenant
                || &facts.scope.execution_id != execution
        }) {
            return Err(Error::Response(
                "provider technical scope identity mismatch",
            ));
        }
        Ok(result)
    }
}
