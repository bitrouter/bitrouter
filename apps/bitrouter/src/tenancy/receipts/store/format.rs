//! Format selection never converts an ordered receipt through a version-one DTO.
use super::Candidate;
use crate::tenancy::receipts::ordered::CandidateV2;
use anyhow::{Result, bail};
use bitrouter_ai::tenancy::{
    execution::ReceiptState,
    identity::{CorrelationId, ExecutionId, InstallationId, PrincipalId, TenantId, Timestamp},
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) enum Format {
    V1,
    V2,
}
impl Format {
    pub fn number(self) -> i32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
    pub fn decode(value: i32) -> Result<Self> {
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            _ => bail!("unsupported receipt format"),
        }
    }
}

pub(in crate::tenancy) struct Facts<'a> {
    pub installation: &'a InstallationId,
    pub tenant: &'a TenantId,
    pub principal: &'a PrincipalId,
    pub execution: &'a ExecutionId,
    pub correlation: &'a CorrelationId,
    pub recorded_at: &'a Timestamp,
    pub state: ReceiptState,
}

/// Immutable terminal obligation. Legacy consumers explicitly request V1.
pub(in crate::tenancy) enum Terminal {
    V1(Arc<Candidate>),
    V2(Arc<CandidateV2>),
}
impl Terminal {
    pub fn format(&self) -> Format {
        match self {
            Self::V1(_) => Format::V1,
            Self::V2(_) => Format::V2,
        }
    }
    pub fn facts(&self) -> Facts<'_> {
        match self {
            Self::V1(c) => {
                let r = &c.receipt;
                Facts {
                    installation: &r.installation_id,
                    tenant: &r.tenant_id,
                    principal: &r.principal_id,
                    execution: &r.execution_id,
                    correlation: &r.correlation_id,
                    recorded_at: &r.recorded_at,
                    state: r.state,
                }
            }
            Self::V2(c) => {
                let r = &c.receipt;
                Facts {
                    installation: &r.installation_id,
                    tenant: &r.tenant_id,
                    principal: &r.principal_id,
                    execution: &r.execution_id,
                    correlation: &r.correlation_id,
                    recorded_at: &r.recorded_at,
                    state: r.state,
                }
            }
        }
    }
    #[cfg(any(test, all(feature = "core", feature = "harness")))]
    pub fn v1(&self) -> Result<&Arc<Candidate>> {
        match self {
            Self::V1(c) => Ok(c),
            Self::V2(_) => bail!("receipt format mismatch"),
        }
    }
    pub fn json(&self) -> &str {
        match self {
            Self::V1(c) => &c.json,
            Self::V2(c) => &c.json,
        }
    }
    pub fn private_json(&self) -> &str {
        match self {
            Self::V1(c) => &c.private_json,
            Self::V2(c) => &c.private_json,
        }
    }
    pub fn digest(&self) -> &str {
        match self {
            Self::V1(c) => &c.digest,
            Self::V2(c) => &c.digest,
        }
    }
    pub fn bytes(&self) -> u64 {
        (self.json().len() + self.private_json().len()) as u64
    }
    pub fn auxiliary_bytes(&self) -> u64 {
        match self {
            Self::V1(c) => c.auxiliary_bytes(),
            Self::V2(c) => c.evidence.json.len() as u64 + 16 * 1024,
        }
    }
    pub fn unknown(&self) -> bool {
        self.facts().state == ReceiptState::Unknown
    }
    pub fn auxiliary_unclean(&self) -> bool {
        matches!(self, Self::V1(c) if c.auxiliary_unclean())
    }
    pub fn has_unknown_outcome(&self) -> bool {
        use bitrouter_ai::tenancy::execution::Outcome;
        self.unknown()
            || match self {
                Self::V1(c) => {
                    c.receipt.outcome == Outcome::Unknown
                        || c.receipt
                            .attempts
                            .iter()
                            .any(|attempt| attempt.outcome == Outcome::Unknown)
                }
                Self::V2(c) => {
                    c.receipt.outcome == Outcome::Unknown
                        || c.receipt
                            .attempts
                            .iter()
                            .any(|attempt| attempt.outcome == Outcome::Unknown)
                }
            }
    }
    pub fn auxiliary_unknown(&self) -> bool {
        matches!(self, Self::V1(c) if c.auxiliary_unknown())
    }
}
