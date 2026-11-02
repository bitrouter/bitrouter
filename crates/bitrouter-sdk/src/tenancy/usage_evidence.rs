//! Versioned execution observations through the existing bounded tenancy transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    usage_evidence::{ExecutionUsageEvidenceV1, MAX_BYTES, UsageEvidenceCapabilitiesV1},
};

impl TenancyClient {
    /// Discover bounded usage evidence independently of frozen v1 capabilities.
    ///
    /// # Errors
    /// Returns authentication, installation, transport or contract errors.
    pub async fn usage_evidence_capabilities(
        &self,
        installation: &InstallationId,
    ) -> Result<UsageEvidenceCapabilitiesV1> {
        let result: UsageEvidenceCapabilitiesV1 = self
            .get("tenancy/usage-evidence/capabilities", Some(installation))
            .await?;
        if &result.installation_id != installation {
            return Err(Error::Response(
                "usage evidence capability installation mismatch",
            ));
        }
        Ok(result)
    }

    /// Read immutable original numeric observations for one exact execution.
    /// Historical absence is explicit; this method never reconstructs usage.
    ///
    /// # Errors
    /// Returns authorization, missing execution, transport or binding errors.
    pub async fn usage_evidence(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<ExecutionUsageEvidenceV1> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{tenant}/executions/{execution}/usage-evidence"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        // Bound the original bytes, including whitespace, before typed decoding.
        let result: ExecutionUsageEvidenceV1 = self.send_bounded(request, MAX_BYTES).await?;
        if &result.installation_id != installation
            || &result.tenant_id != tenant
            || &result.execution_id != execution
        {
            return Err(Error::Response("usage evidence identity mismatch"));
        }
        Ok(result)
    }
}
