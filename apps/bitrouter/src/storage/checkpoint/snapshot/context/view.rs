//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

#[cfg(any(feature = "core", feature = "harness"))]
use super::digest;
use super::evidence::SourceSpan;
#[cfg(any(feature = "core", feature = "harness"))]
use bitrouter_ai::core::protocol::CoreError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Revision commitment excludes unrelated workers, but binds all prompt inputs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRevision {
    pub context_revision: u64,
    pub signal_revision: u64,
    pub manifest_sha256: String,
    pub prompt_sha256: String,
}

/// Each representation retains exact source identities. Full tool groups are
/// atomic; extracts/summaries are labelled evidence text, never forged tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextRepresentation {
    Full {
        block_id: String,
    },
    Extract {
        block_id: String,
        spans: Vec<SourceSpan>,
    },
    Summary {
        artifact_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextView {
    pub view_id: String,
    pub task_id: String,
    pub source: SourceRevision,
    pub source_blocks: Vec<String>,
    pub required: Vec<String>,
    pub selected: Vec<ContextRepresentation>,
    pub omitted: Vec<String>,
    pub decision_id: Option<String>,
    pub reason: String,
    pub prompt_sha256: String,
    pub prompt_bytes: usize,
}

/// A finite candidate set built before semantic evaluation. Noncandidates stay
/// full, so truncating the candidate list never grants omission authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateSet {
    pub task_id: String,
    pub ordered_blocks: Vec<String>,
    pub required: BTreeSet<String>,
    pub candidates: BTreeMap<String, Candidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub block_id: String,
    pub summary_id: Option<String>,
    pub extract_id: Option<String>,
    pub excerpt: String,
}

#[cfg(any(feature = "core", feature = "harness"))]
pub(crate) fn view_identity(view: &ContextView) -> Result<String, CoreError> {
    let mut value = view.clone();
    value.view_id.clear();
    Ok(format!("view_{}", digest(&value)?))
}

pub(crate) fn truncate(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}
