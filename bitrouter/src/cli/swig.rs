//! Swig embedded wallet placeholder functions.
//!
//! These stubs define the interface for Swig interactions. Each function
//! will be replaced with real Swig SDK calls during integration.
//! All signing-related operations require the master wallet's signature,
//! obtained after a passphrase prompt.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Metadata returned after creating a Swig embedded wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedWalletInfo {
    /// The on-chain address of the embedded wallet.
    pub address: String,
    /// Human-readable creation timestamp (ISO 8601).
    pub created_at: String,
}

/// Metadata returned after deriving an agent wallet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWalletInfo {
    /// The on-chain address of the derived agent wallet.
    pub address: String,
    /// Spend permissions associated with this agent wallet.
    pub permissions: AgentPermissions,
    /// Human-readable creation timestamp (ISO 8601).
    pub created_at: String,
}

/// Spend permissions for an agent wallet — enforced by Swig on-chain.
///
/// Local copies of these values are stored for display/reference only;
/// Swig is the source of truth for enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPermissions {
    /// Maximum tokens (lamports / micro-USD) per single transaction.
    pub per_tx_cap: Option<u64>,
    /// Cumulative spending cap across all transactions.
    pub cumulative_cap: Option<u64>,
    /// Unix timestamp after which the agent wallet expires.
    pub expires_at: Option<u64>,
}

/// Create a Swig embedded wallet for the given master wallet.
///
/// Requires the master wallet's signature (passphrase already verified by
/// caller). Returns metadata about the newly created embedded wallet.
///
/// # Placeholder
/// This function will be replaced with an actual Swig SDK call.
pub fn create_embedded_wallet(
    _master_keypair_path: &Path,
    _signature: &[u8],
) -> Result<EmbeddedWalletInfo, String> {
    Err(
        "swig: create_embedded_wallet is not yet implemented — awaiting Swig SDK integration"
            .into(),
    )
}

/// Derive an agent wallet from the embedded wallet with spend permissions.
///
/// Requires the master wallet's signature. The permissions are set on-chain
/// via Swig; the returned metadata includes a local copy for reference.
///
/// # Placeholder
/// This function will be replaced with an actual Swig SDK call.
pub fn derive_agent_wallet(
    _master_keypair_path: &Path,
    _signature: &[u8],
    _permissions: &AgentPermissions,
) -> Result<AgentWalletInfo, String> {
    Err("swig: derive_agent_wallet is not yet implemented — awaiting Swig SDK integration".into())
}

/// Update the spend permissions on an existing agent wallet.
///
/// Requires the master wallet's signature. The new permissions are set
/// on-chain via Swig.
///
/// # Placeholder
/// This function will be replaced with an actual Swig SDK call.
pub fn set_agent_permissions(
    _master_keypair_path: &Path,
    _signature: &[u8],
    _agent_address: &str,
    _permissions: &AgentPermissions,
) -> Result<AgentPermissions, String> {
    Err("swig: set_agent_permissions is not yet implemented — awaiting Swig SDK integration".into())
}
