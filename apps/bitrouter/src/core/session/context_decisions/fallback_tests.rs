//! Fixture support for the real owned session regressions in tenancy.
use super::*;
use crate::storage::checkpoint::snapshot::context::action::ContextAction;

impl CoreSession {
    /// Publish admitted historical derivations at a settled fixture boundary.
    /// Only source data is arranged here: preparation, Choice, receipts, routing
    /// and generation all run through the real owned session in the tests.
    pub(crate) async fn derive_context_fallback_for_test(
        &self,
        action: ContextAction,
    ) -> Result<Prompt, CoreError> {
        self.transition("test.context.derivation", |state, _| {
            if !root_queue::settled(state) {
                return Err(reject(ErrorCode::Busy, "fixture requires settled context"));
            }
            let agent_id = state.agent_id.clone();
            crate::core::context_router::tools::apply(state, &agent_id, &action)
        })
        .await?;
        let state = self.snapshot().await;
        build_prompt(&state, &state.agent_id)
    }
}
