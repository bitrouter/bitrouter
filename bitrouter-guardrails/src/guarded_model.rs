use std::sync::Arc;

use bitrouter_core::models::language::language_model::DynLanguageModel;

use crate::engine::Guardrail;

/// A model wrapper that holds an inner [`DynLanguageModel`] and a shared
/// [`Guardrail`] engine. The [`LanguageModel`](bitrouter_core::models::language::language_model::LanguageModel)
/// implementation lives in `router.rs` so that all trait methods are co-located.
pub struct GuardedModel {
    pub(crate) inner: Box<DynLanguageModel<'static>>,
    pub(crate) guardrail: Arc<Guardrail>,
}

impl GuardedModel {
    pub fn new(inner: Box<DynLanguageModel<'static>>, guardrail: Arc<Guardrail>) -> Self {
        Self { inner, guardrail }
    }
}
