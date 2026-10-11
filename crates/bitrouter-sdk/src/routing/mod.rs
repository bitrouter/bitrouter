//! Shared semantic routing. Input origin is diagnostic; validated signals,
//! admitted capabilities and policy determine the available actions.

pub mod assessment;
pub mod input;
pub mod plan;
pub mod signals;

use serde::{Deserialize, Serialize};

/// Operations granted by the context owner, independent of transport or harness.
/// Merely recognizing a workflow does not grant authority to rewrite its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCapability {
    /// Leave optional source evidence out of one materialized view.
    OmitEvidence,
    /// Substitute exact source spans committed by the context owner.
    UseExtract,
    /// Substitute a source-bound historical summary.
    UseSummary,
    /// Retrieve admitted evidence outside the active view.
    RecallEvidence,
}

pub mod preparation;

/// Context treatment owned by a policy action. It never grants host rights.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Preserve the complete admitted source view.
    Preserve,
    /// Select among semantically justified views within granted capabilities.
    #[default]
    Evidence,
}
