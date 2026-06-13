//! Chainlink confidential attestation for the pay gate — delegates to the shared
//! `bitrouter-chainlink` engine. No duplicate client, no `attested: true`.

// Private module + root re-export (CLAUDE.md rule 2: no re-export inside a
// *public* mod). Callers use `crate::attester::run_attested_inference`.
#[cfg(feature = "arc")]
mod runner;

#[cfg(feature = "arc")]
pub use runner::run_attested_inference;
