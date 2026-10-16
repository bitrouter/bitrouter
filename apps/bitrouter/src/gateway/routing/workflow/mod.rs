//! Serving-time workflow projection, identity and routing evidence.
//!
//! These helpers interpret inference requests and published predictor contracts.
//! They do not train policies or own a harness conversation. Offline analysis
//! consumes the same projections from its separate implementation.

pub mod decision;
pub mod extractors;
pub mod ir;
pub mod online;
pub mod predictive;
pub mod session;
