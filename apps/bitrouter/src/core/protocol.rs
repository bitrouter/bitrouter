//! Checks against the live connection's authenticated ownership grant.
//! Shared durable-record validation lives in application storage.

use crate::storage::checkpoint::serialized_bytes;
use bitrouter_ai::core::protocol::{
    ClientMessage, Command, CoreError, ErrorCode, Limits, OwnershipGrant, VERSION, validate_id,
};

/// Application checks against the connection's authenticated grant and limits.
/// Passing these checks alone does not authorize an operation or commit state.
pub trait ClientMessageExt {
    fn validate(&self, grant: &OwnershipGrant, limits: &Limits) -> Result<(), CoreError>;
}

impl ClientMessageExt for ClientMessage {
    fn validate(&self, grant: &OwnershipGrant, limits: &Limits) -> Result<(), CoreError> {
        if self.version != VERSION {
            return Err(CoreError::rejected(
                ErrorCode::UnsupportedVersion,
                "supported harness version is 1",
            ));
        }
        validate_id(&self.operation_id)?;
        if self.session_id != grant.session_id {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "connection is bound to a different session",
            ));
        }
        if self.execution_epoch != grant.execution_epoch {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "message does not belong to the active grant",
            ));
        }
        let bound = match self.command {
            Command::Bind(_) | Command::Restore(_) | Command::ProviderEvidence(_) => {
                limits.unacknowledged_bytes
            }
            _ => limits.input_bytes,
        };
        if serialized_bytes(self)? > bound {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "control input exceeds negotiated byte bound",
            ));
        }
        if matches!(
            self.command,
            Command::Enqueue(_)
                | Command::Steer { .. }
                | Command::CancelRun { .. }
                | Command::CancelAgent { .. }
                | Command::ResumeQueue
                | Command::Release
        ) && self.expected_state_revision.is_none()
        {
            return Err(CoreError::rejected(
                ErrorCode::StaleRevision,
                "intent mutations require expected_state_revision",
            ));
        }
        Ok(())
    }
}
