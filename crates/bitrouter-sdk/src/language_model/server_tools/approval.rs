//! The approval seam: a policy point evaluated before each router tool call.
//! v1 ships [`AllowAll`]; a deployment can supply a policy that gates by tool,
//! caller, or arguments without touching the loop core.

use async_trait::async_trait;

use super::classify::RouterCall;
use crate::caller::CallerContext;

/// Decides whether a router tool call may execute.
#[async_trait]
pub trait ApprovalPolicy: Send + Sync {
    /// Whether `call` (issued for `caller`) may be executed.
    async fn allow(&self, call: &RouterCall, caller: &CallerContext) -> bool;
}

/// Approves every router tool call — the v1 default.
pub struct AllowAll;

#[async_trait]
impl ApprovalPolicy for AllowAll {
    async fn allow(&self, _call: &RouterCall, _caller: &CallerContext) -> bool {
        true
    }
}
