//! Translate provenance-preserving Core views into shared routing candidates.

use super::planner::{ContextRepresentation, ContextView};
use bitrouter_ai::types::Prompt;
use bitrouter_sdk::routing::{ContextCapability, plan};

pub(crate) fn view(id: &str, view: &ContextView, prompt: &Prompt) -> plan::View {
    let mut requires = std::collections::BTreeSet::new();
    if !view.omitted.is_empty() {
        requires.insert(ContextCapability::OmitEvidence);
    }
    for representation in &view.selected {
        match representation {
            ContextRepresentation::Extract { .. } => {
                requires.insert(ContextCapability::UseExtract);
            }
            ContextRepresentation::Summary { .. } => {
                requires.insert(ContextCapability::UseSummary);
            }
            ContextRepresentation::Full { .. } => {}
        }
    }
    plan::View {
        id: id.into(),
        prompt: prompt.clone(),
        requires,
    }
}
