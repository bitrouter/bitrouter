//! Transport-independent business entry point for request-check capability v1.
//!
//! Both local and HTTP adapters provide the same validated, bounded v1 input.
//! Callbacks do not receive credentials, pipeline state or receipt mutation APIs.

use crate::v1;

/// Business decision returned by a request-check callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDecision {
    /// The projected entry request may proceed.
    Allow,
    /// Stop the request; the reason must follow the v1 reason-code grammar.
    Deny { reason_code: String },
}

/// Ordinary synchronous business function. Hosts must bound its concurrency
/// and run it off async workers. Started callbacks are not forcibly cancellable.
pub type CheckCallback = dyn Fn(&v1::Request) -> CheckDecision + Send + Sync + 'static;

impl CheckDecision {
    /// Validate the business result using the same rules in both adapters.
    pub fn into_response(
        self,
        invocation_id: String,
        implementation_version: String,
    ) -> Result<v1::Response, v1::ProtocolError> {
        match self {
            Self::Allow => v1::Response::allow(invocation_id, Some(implementation_version)),
            Self::Deny { reason_code } => v1::Response::deny(
                invocation_id,
                Some(reason_code),
                Some(implementation_version),
            ),
        }
    }
}
