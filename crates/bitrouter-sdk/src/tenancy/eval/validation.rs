//! Wire identity checks only, not evidence validation or namespace attestation.

use std::collections::BTreeSet;

use bitrouter_ai::eval::{
    AdmissionOutcome, EVAL_SCHEMA_VERSION, EvalSnapshot, EvalSubject, PutSubjectReport,
    StatusReport,
};
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

pub(super) trait EvalReply: DeserializeOwned {
    fn validate_reply(&self) -> Result<()>;
}

impl EvalReply for PutSubjectReport {
    fn validate_reply(&self) -> Result<()> {
        logical_id(&self.eval_id)
    }
}

impl EvalReply for EvalSubject {
    fn validate_reply(&self) -> Result<()> {
        require(self.schema_version == EVAL_SCHEMA_VERSION)?;
        logical_id(&self.eval_id)?;
        logical_id(&self.subject_id)?;
        for decision in &self.decisions {
            logical_id(&decision.decision_id)?;
        }
        for evidence in &self.evidence {
            logical_id(&evidence.evidence_id)?;
        }
        Ok(())
    }
}

impl EvalReply for Vec<EvalSubject> {
    fn validate_reply(&self) -> Result<()> {
        require(self.len() <= 1024)?;
        let mut ids = BTreeSet::new();
        for subject in self {
            subject.validate_reply()?;
            require(ids.insert(subject.eval_id.as_str()))?;
        }
        Ok(())
    }
}

impl EvalReply for AdmissionOutcome {
    fn validate_reply(&self) -> Result<()> {
        // The DTO does not echo eval_id or idempotency_key. Treat result_id as
        // opaque; do not recreate the service's result/content digest algorithm.
        logical_id(&self.result_id)
    }
}

impl EvalReply for EvalSnapshot {
    fn validate_reply(&self) -> Result<()> {
        logical_id(&self.evidence_root)?;
        require(self.entries.len() <= 1024)?;
        for entry in &self.entries {
            logical_id(&entry.result_id)?;
            logical_id(&entry.content_digest)?;
            logical_id(&entry.subject_content_digest)?;
            logical_id(&entry.eval_id)?;
        }
        Ok(())
    }
}

impl EvalReply for StatusReport {
    fn validate_reply(&self) -> Result<()> {
        Ok(())
    }
}

fn logical_id(value: &str) -> Result<()> {
    require(!value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
}

fn require(valid: bool) -> Result<()> {
    if valid {
        Ok(())
    } else {
        Err(Error::Response("invalid evaluation response identity"))
    }
}
