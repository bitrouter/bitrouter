//! Resolve headless ACP permissions using the shared harness policy.

use crate::harness::acp::client::PendingPermission;
use crate::harness::permission::{Decision, Policy, Prompt};

/// Return the actual decision sent to the agent, including missing-option denial.
/// The ACP client's ledger owns cancellation and connection-teardown denial.
pub(crate) fn answer(request: PendingPermission, policy: &Policy) -> (Decision, Prompt) {
    let prompt = Prompt::new(
        request.request_id.clone(),
        request.tool_call.fields.title.clone(),
        request.tool_call.tool_call_id.0.to_string(),
        request.tool_call.fields.kind,
        request.options.clone(),
    );
    let (decision, outcome) = prompt.answer(policy.decide(&prompt));
    request.resolve(outcome);
    (decision, prompt)
}
