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

/// Translate an acknowledged owned Choice outcome into routing evidence.
#[cfg(feature = "harness")]
pub(crate) fn receipt(
    receipt: &crate::storage::checkpoint::snapshot::context::DecisionReceipt,
) -> crate::gateway::routing::preparation::Receipt {
    let outcome = receipt.outcome.as_ref();
    let usage = outcome.and_then(|outcome| match outcome {
        Ok(response) => Some(response.usage),
        Err(error) => error.usage,
    });
    let decoded = outcome
        .and_then(|outcome| outcome.as_ref().ok())
        .map(|response| {
            crate::gateway::routing::assessment::decode(
                &receipt.request,
                response,
                receipt.policy.confidence_threshold,
            )
        });
    let (assessment, error) = match decoded {
        Some(Ok(value)) => (Some(value), None),
        Some(Err(error)) => (None, Some(error)),
        None => (
            None,
            outcome.and_then(|outcome| outcome.as_ref().err()).cloned(),
        ),
    };
    crate::gateway::routing::preparation::Receipt {
        id: receipt.decision_id.clone(),
        assessment,
        error,
        usage,
    }
}
