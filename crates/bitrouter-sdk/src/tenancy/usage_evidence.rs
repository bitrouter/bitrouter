//! Original execution observations through the existing bounded tenancy transport.
use super::TenancyClient;
use crate::error::{Error, Result};
use bitrouter_ai::tenancy::{
    execution::ExecutionReceipt,
    identity::InstallationId,
    usage_evidence::{EvidenceBudget, ExecutionUsageEvidence, UsageEvidenceCapabilities},
    validation::Validate,
};

impl TenancyClient {
    /// Discover bounded usage evidence for strict and ordered dispatch.
    ///
    /// # Errors
    /// Returns authentication, installation, transport or contract errors.
    pub async fn usage_evidence_capabilities(
        &self,
        installation: &InstallationId,
    ) -> Result<UsageEvidenceCapabilities> {
        let result: UsageEvidenceCapabilities = self
            .get("tenancy/usage-evidence/capabilities", Some(installation))
            .await?;
        if &result.installation_id != installation {
            return Err(Error::Response(
                "usage evidence capability installation mismatch",
            ));
        }
        Ok(result)
    }

    /// Read original evidence bound to the observed receipt and its dispatch.
    /// The accepted count bounds original response bytes before decoding; validated
    /// evidence must agree with the receipt's identity, selection and actual attempts.
    /// No receipt discovery, format fallback, reconstruction or retry is performed.
    ///
    /// # Errors
    /// Rejects invalid receipts locally; returns authority, format, bounds or binding errors.
    pub async fn usage_evidence(
        &self,
        receipt: &ExecutionReceipt,
    ) -> Result<ExecutionUsageEvidence> {
        receipt
            .validate()
            .map_err(|_| Error::Request("invalid receipt"))?;
        let budget = EvidenceBudget::for_attempts(receipt.dispatch.accepted_count())
            .map_err(|_| Error::Request("invalid evidence budget"))?;
        let request = self
            .http
            .get(self.url(&format!(
                "tenants/{}/executions/{}/usage-evidence",
                receipt.tenant_id, receipt.execution_id
            ))?)
            .header(
                "Bitrouter-Installation-Id",
                receipt.installation_id.as_str(),
            );
        let result: ExecutionUsageEvidence = self.send_bounded(request, budget.max_bytes).await?;
        result
            .validate_against(receipt)
            .map_err(|_| Error::Response("evidence differs from receipt"))?;
        Ok(result)
    }
}
