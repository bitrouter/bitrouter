//! Fixture support for the real owned session regressions in tenancy.
use super::*;
use crate::storage::checkpoint::snapshot::context::action::ContextAction;
use bitrouter_ai::tenancy::configuration::GenerationDefaults;

impl CoreSession {
    /// Publish admitted historical derivations at a settled fixture boundary.
    /// Only source data is arranged here: preparation, Choice, receipts, routing
    /// and generation all run through the real owned session in the tests.
    pub(crate) async fn derive_context_fallback_for_test(
        &self,
        action: ContextAction,
        defaults: &GenerationDefaults,
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
        let mut prompt = build_prompt(&state, &state.agent_id)?;
        // The real TenantExecution::attach applies these retained defaults
        // before prepare_context captures its source digest and byte bound.
        // build_prompt alone is the pre-admission input, not that canonical input.
        prompt
            .params
            .max_tokens
            .get_or_insert(defaults.max_output_tokens);
        prompt
            .params
            .temperature
            .get_or_insert(defaults.temperature);
        Ok(prompt)
    }
}
