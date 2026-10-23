//! Translate provenance-preserving Core views into shared routing candidates.

use crate::gateway::routing::ContextCapability;
use crate::gateway::routing::plan;
use crate::storage::checkpoint::snapshot::context::view::{ContextRepresentation, ContextView};
use bitrouter_ai::types::Prompt;

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
