//! Read actual Core-owned scope through the existing bounded Core transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    provider_technical_facts::MAX_BYTES,
    provider_technical_facts_v2::ProviderTechnicalFactsV2,
};

impl TenancyClient {
    /// Read optional actual owning pipeline/hop observations. Null also covers
    /// an old retained payload1 without payload2 observations; no backfill or
    /// original inference is attempted. Validate against the original receipt
    /// before interpreting any actual-arm or parent/child association.
    ///
    /// # Errors
    /// Original authorization, unsupported Core route, bounded transfer,
    /// malformed schema or identity errors. This method never retries or sends
    /// a provider request, resumes a stream, or falls back to another source.
    pub async fn provider_technical_facts_v2(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<Option<ProviderTechnicalFactsV2>> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{tenant}/executions/{execution}/provider-technical-facts/v2"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        let result: Option<ProviderTechnicalFactsV2> =
            self.send_bounded(request, MAX_BYTES).await?;
        if result.as_ref().is_some_and(|facts| {
            &facts.installation_id != installation
                || &facts.tenant_id != tenant
                || &facts.execution_id != execution
        }) {
            return Err(Error::Response(
                "provider technical scope identity mismatch",
            ));
        }
        Ok(result)
    }
}
