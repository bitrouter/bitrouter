//! Storage discriminators identify strict or ordered dispatch.
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
    Strict,
    Ordered,
}
impl Format {
    pub fn number(self) -> i32 {
        match self {
            Self::Strict => 1,
            Self::Ordered => 2,
        }
    }
    pub fn decode(value: i32) -> Result<Self> {
        match value {
            1 => Ok(Self::Strict),
            2 => Ok(Self::Ordered),
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

/// Immutable terminal receipt and evidence for the selected dispatch mode.
pub(in crate::tenancy) enum Terminal {
    Strict(Arc<Candidate>),
    Ordered(Arc<CandidateV2>),
}
impl Terminal {
    pub fn format(&self) -> Format {
        match self {
            Self::Strict(_) => Format::Strict,
            Self::Ordered(_) => Format::Ordered,
        }
    }
    pub fn facts(&self) -> Facts<'_> {
        match self {
            Self::Strict(c) => {
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
            Self::Ordered(c) => {
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
    #[cfg(test)]
    pub fn strict(&self) -> Result<&Arc<Candidate>> {
        match self {
            Self::Strict(c) => Ok(c),
            Self::Ordered(_) => bail!("receipt format mismatch"),
        }
    }
    pub fn json(&self) -> &str {
        match self {
            Self::Strict(c) => &c.json,
            Self::Ordered(c) => &c.json,
        }
    }
    pub fn private_json(&self) -> &str {
        match self {
            Self::Strict(c) => &c.private_json,
            Self::Ordered(c) => &c.private_json,
        }
    }
    pub fn digest(&self) -> &str {
        match self {
            Self::Strict(c) => &c.digest,
            Self::Ordered(c) => &c.digest,
        }
    }
    pub fn bytes(&self) -> u64 {
        (self.json().len() + self.private_json().len()) as u64
    }
    pub fn auxiliary_bytes(&self) -> u64 {
        match self {
            Self::Strict(c) => c.auxiliary_bytes(),
            Self::Ordered(c) => c.evidence.json.len() as u64 + 16 * 1024,
        }
    }
    pub fn unknown(&self) -> bool {
        self.facts().state == ReceiptState::Unknown
    }
    pub fn auxiliary_unclean(&self) -> bool {
        matches!(self, Self::Strict(c) if c.auxiliary_unclean())
    }
    pub fn has_unknown_outcome(&self) -> bool {
        use bitrouter_ai::tenancy::execution::Outcome;
        self.unknown()
            || match self {
                Self::Strict(c) => {
                    c.receipt.outcome == Outcome::Unknown
                        || c.receipt
                            .attempts
                            .iter()
                            .any(|attempt| attempt.outcome == Outcome::Unknown)
                }
                Self::Ordered(c) => {
                    c.receipt.outcome == Outcome::Unknown
                        || c.receipt
                            .attempts
                            .iter()
                            .any(|attempt| attempt.outcome == Outcome::Unknown)
                }
            }
    }
    pub fn auxiliary_unknown(&self) -> bool {
        matches!(self, Self::Strict(c) if c.auxiliary_unknown())
    }
}
