//! Compiled adapters for legacy global and output guardrail hooks.
//!
//! Enabling this module does not register hooks or activate checker rules.

pub mod hooks;
pub mod plugin;

#[cfg(test)]
mod tests;
