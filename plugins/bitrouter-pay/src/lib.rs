//! # bitrouter-pay
//!
//! Arc testnet payment plugin: OWS-backed signing, Proceeds x402/MPP client
//! flows, and optional Chainlink Confidential AI attestation.

#![forbid(unsafe_code)]

#[cfg(feature = "arc")]
pub mod attester;
#[cfg(feature = "arc")]
pub mod chain;
#[cfg(feature = "arc")]
pub mod gate;
pub mod payment;
#[cfg(feature = "arc")]
pub mod plugin;
#[cfg(feature = "arc")]
pub mod wallet;

#[derive(Debug, thiserror::Error)]
pub enum PayError {
    #[error("upstream returned error: {0}")]
    UpstreamError(String),
    #[error("payment failed: {0}")]
    PaymentFailed(String),
    #[error("invalid 402 challenge: {0}")]
    InvalidChallenge(String),
    #[error("attestation failed: {0}")]
    AttestError(String),
    #[error("signer error: {0}")]
    SignerError(String),
    #[error("timeout")]
    Timeout,
}

#[cfg(feature = "arc")]
pub use attester::run_attested_inference;
#[cfg(feature = "arc")]
pub use chain::arc::{
    AGENT_WALLET_ADDRESS, ARC_TESTNET_CAIP2, ARC_TESTNET_CHAIN_ID, ARC_TESTNET_RPC,
    ARC_TESTNET_USDC,
};
#[cfg(feature = "arc")]
pub use gate::{ArcPaymentGate, ArcPaymentGateConfig};
#[cfg(feature = "mpp")]
pub use payment::mpp::{ArcMppBackend, MppBackend, MppClient};
#[cfg(feature = "x402")]
pub use payment::x402::X402Client;
#[cfg(feature = "arc")]
pub use plugin::{DepositPaymentGateHook, PayPlugin, PaymentGateExtension};
#[cfg(feature = "arc")]
pub use wallet::ArcSigner;
