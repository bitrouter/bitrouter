//! Bounded ordered-dispatch usage observations, without auxiliary executions.
//!
//! Selection binds the ingress protocol; each frame records the actual upstream
//! protocol, which may differ after translation. Missing counters stay absent.

use serde::{Deserialize, Serialize};

use super::{
    execution::Target,
    execution_v2::ExecutionReceiptV2,
    identity::{AuthorityRevision, ExecutionId, InstallationId, TenantId},
    ordered::{AcceptedSelectionV2, GenerationProtocol, MAX_ATTEMPTS, validate_tenant_reference},
    usage_evidence::{
        AttemptEvidence, Capture, Estimate, Estimation, Frame, Protocol, validate_observations,
    },
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};

/// Absolute encoded evidence ceiling at 64 accepted candidates.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Absolute frame-count ceiling at 64 accepted candidates.
pub const MAX_FRAMES: usize = 4096;

/// Pure reservation bounds computed from the accepted count, not actual sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidenceBudgetV2 {
    /// Total encoded bytes: 256 KiB per block of eight accepted candidates.
    pub max_bytes: usize,
    /// Frame array bytes: 192 KiB per block of eight accepted candidates.
    pub max_frame_bytes: usize,
    /// Reserved identity/terminal bytes: 64 KiB per block of eight candidates.
    pub reserved_bytes: usize,
    /// Frames per execution: 512 per block of eight accepted candidates.
    pub max_frames: usize,
}
impl EvidenceBudgetV2 {
    /// Compute the evidence reservation for the whole accepted selection.
    ///
    /// # Errors
    /// Rejects counts outside 1–64, including an empty selection.
    pub fn for_attempts(accepted_count: usize) -> Result<Self, ValidationError> {
        require(
            (1..=MAX_ATTEMPTS).contains(&accepted_count),
            "evidence accepted count",
        )?;
        let blocks = accepted_count.div_ceil(8);
        Ok(Self {
            max_bytes: 256 * 1024 * blocks,
            max_frame_bytes: 192 * 1024 * blocks,
            reserved_bytes: 64 * 1024 * blocks,
            max_frames: 512 * blocks,
        })
    }
}

/// Independently versioned original observations for a version-two receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionUsageEvidenceV2 {
    /// Evidence format, exactly two.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Bound authority store.
    pub installation_id: InstallationId,
    /// Bound owning tenant.
    pub tenant_id: TenantId,
    /// Bound actual invocation.
    pub execution_id: ExecutionId,
    /// Exact receipt selection, including the server-observed ingress protocol.
    pub accepted_selection: AcceptedSelectionV2,
    /// Capture status; unavailable recovery never manufactures observations.
    pub capture: Capture,
    /// One for ordinary protocols; two for native ingress or captured native frames.
    #[serde(deserialize_with = "super::number::integer")]
    pub extractor_version: u8,
    /// Whether the existing stream estimator was actually used.
    pub estimation: Estimation,
    /// Required null unless captured estimator provenance is present.
    #[serde(deserialize_with = "nullable")]
    pub estimate: Option<Estimate>,
    /// Every actual receipt attempt in order when captured, including frameless attempts.
    pub attempts: Vec<AttemptEvidence>,
    /// Bounded prefix of original observations. No child or auxiliary linkage exists.
    pub frames: Vec<Frame>,
    /// Sticky whenever any observation was dropped.
    pub truncated: bool,
    /// Saturating exact discarded-observation count.
    #[serde(deserialize_with = "super::number::integer")]
    pub dropped_frames: u64,
}
impl Validate for ExecutionUsageEvidenceV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 2
                && self.extractor_version
                    == Self::extractor_version_for(
                        self.accepted_selection.inbound_protocol,
                        &self.frames,
                    ),
            "ordered evidence version",
        )?;
        self.accepted_selection.validate()?;
        validate_tenant_reference(&self.accepted_selection.route, &self.tenant_id)?;
        let accepted_count = self.accepted_selection.candidate_indices.len();
        let budget = EvidenceBudgetV2::for_attempts(accepted_count)?;
        unique(
            &self
                .attempts
                .iter()
                .map(|a| &a.attempt_id)
                .collect::<Vec<_>>(),
            0,
            accepted_count,
        )?;
        require(
            self.frames.len() <= budget.max_frames && self.dropped_frames <= AuthorityRevision::MAX,
            "usage evidence bounds",
        )?;
        require(
            self.truncated == (self.dropped_frames > 0),
            "usage evidence truncation",
        )?;
        require(
            (self.estimation == Estimation::Captured) == self.estimate.is_some(),
            "estimator presence",
        )?;
        if let Some(estimate) = &self.estimate {
            estimate.validate()?;
        }
        if self.capture != Capture::Captured {
            require(
                self.attempts.is_empty()
                    && self.frames.is_empty()
                    && !self.truncated
                    && self.estimation == Estimation::NotCaptured,
                "absent evidence cannot assert observations",
            )?;
        }
        require(
            self.frames
                .iter()
                .all(|frame| frame.protocol != Protocol::Decisions),
            "ordered evidence protocol",
        )?;
        require(
            self.frames
                .iter()
                .filter(|frame| frame.protocol == Protocol::GenerateContent)
                .all(|frame| {
                    frame
                        .quantities
                        .iter()
                        .all(|quantity| quantity.value.is_none_or(|value| value <= i32::MAX as u64))
                }),
            "native quantity range",
        )?;
        validate_observations(&self.attempts, &self.frames)?;
        bytes(&self.frames, budget.max_frame_bytes)?;
        bytes(self, budget.max_bytes)
    }
}
impl ExecutionUsageEvidenceV2 {
    /// Select the exact mapping discriminator without changing historical documents.
    /// Native ingress remains version two even when capture has no retained frames.
    pub fn extractor_version_for(inbound: GenerationProtocol, frames: &[Frame]) -> u8 {
        if inbound == GenerationProtocol::GenerateContent
            || frames
                .iter()
                .any(|frame| frame.protocol == Protocol::GenerateContent)
        {
            2
        } else {
            1
        }
    }

    /// Validate evidence and bind its identities, selection and actual attempts
    /// to the immutable receipt. Capture absence remains explicit after recovery.
    ///
    /// # Errors
    /// Rejects invalid documents, identity/selection mismatch, or any missing,
    /// reordered or fabricated captured attempt, target or outcome.
    pub fn validate_against(&self, receipt: &ExecutionReceiptV2) -> Result<(), ValidationError> {
        self.validate()?;
        receipt.validate()?;
        require(
            self.installation_id == receipt.installation_id
                && self.tenant_id == receipt.tenant_id
                && self.execution_id == receipt.execution_id
                && self.accepted_selection == receipt.accepted_selection,
            "ordered evidence receipt binding",
        )?;
        if self.capture == Capture::Captured {
            require(
                self.attempts.len() == receipt.attempts.len(),
                "ordered evidence actual attempts",
            )?;
            for (evidence, attempt) in self.attempts.iter().zip(&receipt.attempts) {
                require(
                    evidence.attempt_id == attempt.attempt_id
                        && evidence.outcome == attempt.outcome
                        && same_target(evidence.target.as_ref(), attempt.target.as_ref()),
                    "ordered evidence attempt binding",
                )?;
            }
        }
        Ok(())
    }
}

fn same_target(left: Option<&Target>, right: Option<&Target>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.provider_id == right.provider_id
                && left.model_id == right.model_id
                && left.account_id == right.account_id
                && left.credential_id == right.credential_id
                && left.credential_version == right.credential_version
        }
        _ => false,
    }
}
