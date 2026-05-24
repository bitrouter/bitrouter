//! Phase 1 placeholder. The real implementation lands in Phase 2.

#![allow(dead_code)]

/// Provider id this applier is registered under once Phase 2 lands.
pub const PROVIDER_ID: &str = "bitrouter-cloud";

/// `AuthApplier` for `provider_name == "bitrouter-cloud"`. Phase 2 will fill
/// this in — the struct is exposed here so the lib's public surface stays
/// stable across phases.
pub struct BitrouterCloudAuthApplier;
