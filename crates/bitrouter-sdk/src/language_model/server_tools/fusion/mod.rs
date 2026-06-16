//! Fusion: a multi-model deliberation server tool. A panel of models answers a
//! prompt in parallel, a judge compares (not merges) their answers into
//! structured analysis, and the calling model writes the final answer from it.
//!
//! Reference design (behavior modeled after OpenRouter Fusion):
//! <https://openrouter.ai/docs/guides/features/server-tools/fusion>

pub mod config;
pub mod engine;
pub mod judge;
