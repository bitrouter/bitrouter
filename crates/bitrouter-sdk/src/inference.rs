//! Operation identity shared by provider configuration and model selection.
//!
//! Operations describe different request/result contracts, not optional
//! capabilities of one generation request.

use serde::{Deserialize, Serialize};

/// The semantic operation a concrete provider/model route can perform.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InferenceOperation {
    /// Generate a language-model response from a prompt.
    Generate,
    /// Answer typed questions about a state.
    Evaluate,
}
