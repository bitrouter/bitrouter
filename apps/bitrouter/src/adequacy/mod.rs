//! Read-only compatibility surface for pre-v2 learned evidence.
//!
//! Legacy pins, exploration counters, and semantic successes are sealed inputs
//! to the v2 migration compiler. New observations enter the generic eval
//! exchange; none of these modules participates in semantic route selection.

pub mod reliability;
pub mod report;
pub mod store;
