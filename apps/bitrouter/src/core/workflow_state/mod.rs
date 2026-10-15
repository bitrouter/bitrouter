//! Workflow research, replay and learning consumers.
//!
//! Request-time projections and published predictor validation belong to
//! `gateway::routing::workflow`, independently of these analysis services.

pub mod archive;
pub mod classifier_bakeoff;
pub mod classifier_baseline;
pub mod counterfactual;
pub mod fixture;
pub mod measurement;
pub mod real_trace;
pub mod replay;
pub mod reward;
pub mod reward_feedback;
pub mod shadow_classifier;
pub mod shadow_policy;
