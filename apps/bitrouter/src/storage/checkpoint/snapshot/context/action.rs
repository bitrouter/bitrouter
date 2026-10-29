//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", content = "arguments", deny_unknown_fields)]
pub enum ContextAction {
    #[serde(rename = "context_search")]
    Search { query: String },
    #[serde(rename = "context_recall")]
    Recall { block_ids: Vec<String> },
    #[serde(rename = "context_publish")]
    Publish {
        block_ids: Vec<String>,
        summary: String,
    },
    #[serde(rename = "context_extract")]
    Extract {
        block_id: String,
        spans: Vec<crate::storage::checkpoint::snapshot::context::evidence::SourceSpan>,
    },
}
