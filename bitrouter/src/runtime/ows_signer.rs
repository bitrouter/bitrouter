//! OWS-backed signer for MPP payment operations.
//!
//! [`OwsSigner`] implements `alloy::signers::Signer` using the Open Wallet
//! Standard for key management. Private keys remain encrypted at rest and are
//! decrypted only during the signing operation, then immediately wiped from
//! memory by OWS's zeroizing layer.
//!
//! This replaces raw hex private keys (`mpp::PrivateKeySigner`) for
//! server-side MPP close signing when an OWS wallet is configured.

use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256, ChainId, Signature};

/// EVM signer backed by an OWS wallet.
///
/// At construction the wallet's EVM address is resolved (no passphrase
/// required). For each [`sign_hash`](alloy::signers::Signer::sign_hash)
/// call the wallet is decrypted, the hash is signed, and the key material
/// is zeroized on drop.
pub struct OwsSigner {
    wallet_name: String,
    credential: String,
    index: Option<u32>,
    vault_path: Option<PathBuf>,
    address: Address,
    chain_id: Option<ChainId>,
}

impl OwsSigner {
    /// Create a new OWS signer for the given wallet.
    ///
    /// Resolves the wallet's EVM address immediately. Returns an error if
    /// the wallet does not exist or has no EVM account.
    pub fn new(
        wallet_name: &str,
        credential: &str,
        index: Option<u32>,
        vault_path: Option<&Path>,
        chain_id: Option<ChainId>,
    ) -> Result<Self, OwsSignerError> {
        let info = ows_lib::get_wallet(wallet_name, vault_path)
            .map_err(|e| OwsSignerError::WalletNotFound(format!("{wallet_name}: {e}")))?;

        let evm_account = info
            .accounts
            .iter()
            .find(|a| a.chain_id.starts_with("eip155:"))
            .ok_or_else(|| {
                OwsSignerError::NoEvmAccount(format!("wallet '{wallet_name}' has no EVM account"))
            })?;

        let address: Address = evm_account
            .address
            .parse()
            .map_err(|e| OwsSignerError::InvalidAddress(format!("{}: {e}", evm_account.address)))?;

        Ok(Self {
            wallet_name: wallet_name.to_string(),
            credential: credential.to_string(),
            index,
            vault_path: vault_path.map(Path::to_path_buf),
            address,
            chain_id,
        })
    }

    /// Sign a 32-byte prehash using the OWS wallet.
    ///
    /// Decrypts the key, signs, and zeroizes in one step.
    fn sign_prehash(&self, hash: &[u8; 32]) -> Result<Signature, alloy::signers::Error> {
        let chain_type = ows_core::ChainType::Evm;

        let key = ows_lib::decrypt_signing_key(
            &self.wallet_name,
            chain_type,
            &self.credential,
            self.index,
            self.vault_path.as_deref(),
        )
        .map_err(|e| alloy::signers::Error::other(e.to_string()))?;

        let signer = ows_signer::signer_for_chain(chain_type);
        let output = signer
            .sign(key.expose(), hash)
            .map_err(|e| alloy::signers::Error::other(e.to_string()))?;
        // `key` (SecretBytes) is zeroized on drop here.

        to_alloy_signature(&output)
    }
}

#[async_trait::async_trait]
impl alloy::signers::Signer for OwsSigner {
    async fn sign_hash(&self, hash: &B256) -> alloy::signers::Result<Signature> {
        // Scrypt decryption is CPU-bound; avoid blocking the async runtime.
        let hash_bytes: [u8; 32] = hash.0;
        let wallet_name = self.wallet_name.clone();
        let credential = self.credential.clone();
        let index = self.index;
        let vault_path = self.vault_path.clone();
        let chain_id = self.chain_id;

        tokio::task::spawn_blocking(move || {
            let signer = OwsSigner {
                wallet_name,
                credential,
                index,
                vault_path,
                // address and chain_id are not used inside sign_prehash
                address: Address::ZERO,
                chain_id,
            };
            signer.sign_prehash(&hash_bytes)
        })
        .await
        .map_err(|e| alloy::signers::Error::other(format!("blocking task failed: {e}")))?
    }

    fn address(&self) -> Address {
        self.address
    }

    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id
    }

    fn set_chain_id(&mut self, chain_id: Option<ChainId>) {
        self.chain_id = chain_id;
    }
}

/// Convert OWS `SignOutput` (65-byte `r || s || v` for secp256k1) into
/// an alloy [`Signature`].
fn to_alloy_signature(output: &ows_signer::SignOutput) -> Result<Signature, alloy::signers::Error> {
    if output.signature.len() != 65 {
        return Err(alloy::signers::Error::other(format!(
            "expected 65-byte signature, got {}",
            output.signature.len()
        )));
    }

    let r = B256::from_slice(&output.signature[..32]);
    let s = B256::from_slice(&output.signature[32..64]);
    let v = output
        .recovery_id
        .ok_or_else(|| alloy::signers::Error::other("missing recovery id"))?;

    // OWS returns raw recovery_id (0 or 1).
    let v_parity = v & 1 != 0;
    Ok(Signature::new(
        alloy::primitives::U256::from_be_bytes(r.0),
        alloy::primitives::U256::from_be_bytes(s.0),
        v_parity,
    ))
}

/// Errors that can occur when constructing an [`OwsSigner`].
#[derive(Debug, thiserror::Error)]
pub enum OwsSignerError {
    #[error("wallet not found: {0}")]
    WalletNotFound(String),

    #[error("wallet has no EVM account: {0}")]
    NoEvmAccount(String),

    #[error("invalid EVM address: {0}")]
    InvalidAddress(String),
}

impl std::fmt::Display for OwsSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OwsSigner(wallet={}, address={})",
            self.wallet_name, self.address
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_roundtrip() {
        // Verify our conversion produces a valid alloy Signature from
        // a synthetic 65-byte r||s||v blob.
        let mut sig_bytes = vec![0u8; 65];
        // Non-zero r and s so the signature is structurally valid.
        sig_bytes[31] = 1; // r = 1
        sig_bytes[63] = 2; // s = 2
        sig_bytes[64] = 0; // recovery_id = 0

        let output = ows_signer::SignOutput {
            signature: sig_bytes,
            recovery_id: Some(0),
            public_key: None,
        };

        let sig = to_alloy_signature(&output).expect("should convert");
        assert!(!sig.v());
    }

    #[test]
    fn signature_with_recovery_id_1() {
        let mut sig_bytes = vec![0u8; 65];
        sig_bytes[31] = 1;
        sig_bytes[63] = 2;
        sig_bytes[64] = 1;

        let output = ows_signer::SignOutput {
            signature: sig_bytes,
            recovery_id: Some(1),
            public_key: None,
        };

        let sig = to_alloy_signature(&output).expect("should convert");
        assert!(sig.v());
    }

    #[test]
    fn rejects_wrong_length_signature() {
        let output = ows_signer::SignOutput {
            signature: vec![0u8; 64],
            recovery_id: Some(0),
            public_key: None,
        };

        assert!(to_alloy_signature(&output).is_err());
    }

    #[test]
    fn rejects_missing_recovery_id() {
        let output = ows_signer::SignOutput {
            signature: vec![0u8; 65],
            recovery_id: None,
            public_key: None,
        };

        assert!(to_alloy_signature(&output).is_err());
    }
}
