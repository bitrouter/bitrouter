//! Explicit discovery and bounded readback for ordered dispatch.

use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution_v2::{self, ExecutionReceiptV2},
    identity::{ExecutionId, InstallationId, RevisionId, TenantId},
    ordered::{CandidateManifestV1, GenerationProtocol, MAX_MANIFEST_BYTES, OrderedCapabilitiesV2},
    usage_evidence_v2::{EvidenceBudgetV2, ExecutionUsageEvidenceV2},
    validation::Validate,
};

use super::{TenancyClient, transport};
use crate::error::{Error, Result};

impl TenancyClient {
    /// Discover ordered v2 support independently of frozen v1 capabilities.
    /// No installation is assumed before discovery; the returned identity binds later calls.
    ///
    /// # Errors
    /// Returns authentication, unsupported version, bounded transport or contract errors.
    pub async fn ordered_capabilities(&self) -> Result<OrderedCapabilitiesV2> {
        self.get("tenancy/ordered/capabilities", None).await
    }

    /// Read an operator-only nonsecret manifest of one exact retained protocol root.
    /// The result confers no dispatch authority and never expands a caller selection.
    ///
    /// # Errors
    /// Rejects a non-route reference locally; returns authority, bounds or binding errors.
    pub async fn candidate_manifest(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        revision: &RevisionId,
        route: &ResourceRef,
        protocol: GenerationProtocol,
    ) -> Result<CandidateManifestV1> {
        if route.resource_kind != ResourceKind::Route {
            return Err(Error::Request(
                "candidate manifest requires a route reference",
            ));
        }
        let mut url = self.url(&format!(
            "tenancy/tenants/{tenant}/configurations/{revision}/candidates"
        ))?;
        let owner_kind = match route.owner_kind {
            OwnerKind::Tenant => "tenant",
            OwnerKind::OperatorPool => "operator_pool",
        };
        let inbound_protocol = match protocol {
            GenerationProtocol::ChatCompletions => "chat_completions",
            GenerationProtocol::Messages => "messages",
            GenerationProtocol::Responses => "responses",
        };
        url.query_pairs_mut()
            .append_pair("route_owner_kind", owner_kind)
            .append_pair("route_owner_id", route.owner_id.as_str())
            .append_pair("route_id", route.resource_id.as_str())
            .append_pair("route_revision", route.revision_id.as_str())
            .append_pair("inbound_protocol", inbound_protocol);
        let result: CandidateManifestV1 = self
            .send_bounded(
                self.http
                    .get(url)
                    .header("Bitrouter-Installation-Id", installation.as_str()),
                MAX_MANIFEST_BYTES,
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.tenant_id == *tenant
                && result.execution_revision == *revision
                && result.route == *route
                && result.inbound_protocol == protocol,
        )?;
        Ok(result)
    }

    /// Read an immutable v2 receipt explicitly; never converts or truncates v1 facts.
    ///
    /// # Errors
    /// Returns current receipt authority, version mismatch, byte bounds or identity errors.
    pub async fn execution_v2(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        execution: &ExecutionId,
    ) -> Result<ExecutionReceiptV2> {
        let request = self
            .http
            .get(self.url(&format!(
                "tenancy/tenants/{tenant}/executions/{execution}/v2"
            ))?)
            .header("Bitrouter-Installation-Id", installation.as_str());
        let result: ExecutionReceiptV2 =
            self.send_bounded(request, execution_v2::MAX_BYTES).await?;
        transport::response_matches(
            result.installation_id == *installation
                && result.tenant_id == *tenant
                && result.execution_id == *execution,
        )?;
        Ok(result)
    }

    /// Read v2 evidence bound to an explicitly observed v2 receipt and its selection.
    /// The accepted count bounds original response bytes before decoding; validated
    /// evidence must agree with the receipt's identity, selection and actual attempts.
    /// No receipt discovery, format fallback, reconstruction or retry is performed.
    ///
    /// # Errors
    /// Rejects invalid receipts locally; returns authority, format, bounds or binding errors.
    pub async fn usage_evidence_v2(
        &self,
        receipt: &ExecutionReceiptV2,
    ) -> Result<ExecutionUsageEvidenceV2> {
        receipt
            .validate()
            .map_err(|_| Error::Request("invalid ordered receipt"))?;
        let budget =
            EvidenceBudgetV2::for_attempts(receipt.accepted_selection.candidate_indices.len())
                .map_err(|_| Error::Request("invalid ordered evidence budget"))?;
        let request = self
            .http
            .get(self.url(&format!(
                "tenancy/tenants/{}/executions/{}/usage-evidence/v2",
                receipt.tenant_id, receipt.execution_id
            ))?)
            .header(
                "Bitrouter-Installation-Id",
                receipt.installation_id.as_str(),
            );
        let result: ExecutionUsageEvidenceV2 = self.send_bounded(request, budget.max_bytes).await?;
        result
            .validate_against(receipt)
            .map_err(|_| Error::Response("ordered evidence differs from receipt"))?;
        Ok(result)
    }
}
