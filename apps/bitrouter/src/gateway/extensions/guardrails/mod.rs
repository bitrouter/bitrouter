//! Compiled adapters for native request checks and global/output guardrail hooks.
//!
//! Enabling this module does not register hooks or activate checker rules.

pub mod hooks;
pub mod plugin;

#[cfg(test)]
mod tests;

/// Native input-only regex request checks.
pub mod checker;
