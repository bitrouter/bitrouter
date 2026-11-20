//! Read actual Core-owned scope through the existing bounded Core transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    provider_technical_facts::MAX_BYTES,
    provider_technical_facts::ProviderTechnicalFacts,
};

impl TenancyClient {
    /// Read optional provider operation, pipeline, hop and observer measurements.
    /// Call `ProviderTechnicalFacts::validate_against` with the original
    /// `ExecutionReceipt` before interpreting actual-arm or parent/child
    /// associations. Its dispatch supplies the strict or ordered attempt limits.
    ///
    /// # Errors
    /// Original authorization, unsupported Core route, bounded transfer,
    /// malformed schema or identity errors. This method never retries or sends
    /// a provider request, resumes a stream, or falls back to another source.
    pub async fn provider_technical_facts(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<Option<ProviderTechnicalFacts>> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{tenant}/executions/{execution}/provider-technical-facts"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        let result: Option<ProviderTechnicalFacts> = self.send_bounded(request, MAX_BYTES).await?;
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
