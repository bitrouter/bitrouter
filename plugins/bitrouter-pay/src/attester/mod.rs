//! Chainlink confidential attestation for the pay gate — delegates to the shared
//! `bitrouter-chainlink` engine. No duplicate client, no `attested: true`.

#[cfg(feature = "arc")]
pub mod runner;

#[cfg(feature = "arc")]
pub use runner::run_attested_inference;
