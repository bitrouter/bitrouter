//! Raw-key signer for the MPP on-chain settlement path.
//!
//! [`ArcSigner`](super::ows_signer::ArcSigner) delegates to the OWS CLI, which
//! only signs EIP-712 typed data — its `sign_hash` returns an error by design.
//! The MPP charge flow (a Tempo TIP-20 transfer) must sign a bare transaction
//! hash, so this module loads the `agent-treasury` mnemonic directly from its
//! OWS vault file, decrypts it (scrypt + AES-256-GCM, the OWS v2 envelope), and
//! derives the secp256k1 key at the EVM account path. Hash signing is then done
//! locally with alloy.
//!
//! This signer is wired ONLY into [`ArcMppBackend`](crate::ArcMppBackend) for
//! MPP settlement. x402 EIP-712 payments still go through `ArcSigner` and the
//! OWS CLI — that path is untouched.
//!
//! # Scope: this signer does NOT build or encode transactions
//!
//! `ArcLocalSigner` only implements [`Signer::sign_hash`] — it signs whatever
//! 32-byte hash `mpp-br` hands it. It has no control over the transaction
//! *type* or *encoding*: that is entirely owned by `mpp-br`'s `tempo` method
//! (`TempoCharge` → `TempoTransaction` → `sign_and_encode_async`), which emits
//! a Tempo account-abstraction **typed envelope** (`0x76`, or `0x78` in
//! fee-payer mode) wrapping ITIP20 precompile calls — NOT an EIP-155 legacy or
//! EIP-1559 RLP transaction. That envelope only decodes on a Tempo chain
//! (`4217` mainnet / `42431` Moderato). Broadcasting it to a generic EVM chain
//! like Arc testnet (`5042002`) yields the RPC error
//! `-32602: failed to decode signed transaction`. No change to `sign_hash` or
//! the chain id here can alter that — settling MPP on Arc requires either a
//! Tempo settlement chain or a different (EIP-3009-style) payment method.

use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256, ChainId, Signature};
use alloy::signers::Signer;
use mpp_br::PrivateKeySigner;
use ows_signer::crypto::{CryptoEnvelope, decrypt};
use ows_signer::curve::Curve;
use ows_signer::hd::HdDeriver;
use ows_signer::mnemonic::Mnemonic;
use ows_signer::zeroizing::SecretBytes;
use serde::Deserialize;

use crate::PayError;
use crate::chain::arc::ARC_TESTNET_CHAIN_ID;

/// Default OWS vault file holding the `agent-treasury` wallet. Override with
/// `BITROUTER_MPP_VAULT_FILE`.
const DEFAULT_VAULT_FILE: &str =
    "/home/maka/.ows/wallets/7e307e2b-d052-4bd5-a5a8-ce7dc9081b21.json";

/// Documented vault passphrase (per the task brief). Some OWS wallets are
/// created with an empty passphrase (`--no-passphrase`); both are attempted when
/// `OWS_PASSPHRASE` is not set explicitly.
const DOCUMENTED_PASSPHRASE: &str = "L33tC0d31337@!@!@@!";

/// Standard EVM (eip155) BIP-44 account path, used when the vault account record
/// omits its `derivation_path`.
const DEFAULT_EVM_PATH: &str = "m/44'/60'/0'/0/0";

/// Minimal view of an OWS v2 vault file: the encrypted envelope plus account
/// records (used to resolve the EVM derivation path).
#[derive(Debug, Deserialize)]
struct VaultFile {
    crypto: CryptoEnvelope,
    #[serde(default)]
    accounts: Vec<VaultAccount>,
}

#[derive(Debug, Deserialize)]
struct VaultAccount {
    chain_id: String,
    #[serde(default)]
    derivation_path: Option<String>,
}

/// Local raw-key signer used for MPP on-chain settlement on Arc testnet.
///
/// Wraps an alloy [`PrivateKeySigner`] derived from the decrypted vault
/// mnemonic. Unlike `ArcSigner`, this implements a working
/// [`sign_hash`](Signer::sign_hash).
#[derive(Clone)]
pub struct ArcLocalSigner {
    inner: PrivateKeySigner,
    address: Address,
    chain_id: ChainId,
}

impl ArcLocalSigner {
    /// Load the `agent-treasury` signer from the default OWS vault file.
    ///
    /// Honours the `BITROUTER_MPP_VAULT_FILE` and `OWS_PASSPHRASE` environment
    /// overrides. This runs scrypt key derivation and is therefore CPU-bound;
    /// callers on an async runtime should invoke it via `spawn_blocking`.
    pub fn agent_treasury() -> Result<Self, PayError> {
        let path = std::env::var("BITROUTER_MPP_VAULT_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_VAULT_FILE));
        Self::from_vault_file(&path)
    }

    /// Load and decrypt an OWS vault file, deriving the EVM signing key.
    pub fn from_vault_file(path: &Path) -> Result<Self, PayError> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            PayError::SignerError(format!(
                "failed to read OWS vault file {}: {e}",
                path.display()
            ))
        })?;
        let vault: VaultFile = serde_json::from_str(&contents).map_err(|e| {
            PayError::SignerError(format!("invalid OWS vault JSON in {}: {e}", path.display()))
        })?;

        let phrase = decrypt_mnemonic(&vault.crypto)?;
        let phrase_str = std::str::from_utf8(phrase.expose()).map_err(|e| {
            PayError::SignerError(format!("decrypted mnemonic is not valid UTF-8: {e}"))
        })?;
        let mnemonic = Mnemonic::from_phrase(phrase_str)
            .map_err(|e| PayError::SignerError(format!("invalid mnemonic: {e}")))?;

        let derivation_path = vault
            .accounts
            .iter()
            .find(|a| a.chain_id.starts_with("eip155:"))
            .and_then(|a| a.derivation_path.clone())
            .unwrap_or_else(|| DEFAULT_EVM_PATH.to_string());

        // The BIP-39 seed passphrase (distinct from the vault encryption
        // passphrase) is empty for OWS wallets.
        let key =
            HdDeriver::derive_from_mnemonic(&mnemonic, "", &derivation_path, Curve::Secp256k1)
                .map_err(|e| PayError::SignerError(format!("HD derivation failed: {e}")))?;

        let chain_id: ChainId = ARC_TESTNET_CHAIN_ID;
        let inner = PrivateKeySigner::from_slice(key.expose())
            .map_err(|e| PayError::SignerError(format!("invalid secp256k1 private key: {e}")))?
            .with_chain_id(Some(chain_id));
        let address = inner.address();

        Ok(Self {
            inner,
            address,
            chain_id,
        })
    }

    /// Resolved EVM address for the wallet's eip155 account.
    pub fn address(&self) -> Address {
        self.address
    }
}

/// Decrypt the vault `crypto` envelope, trying the configured passphrase
/// candidates in order, and return the plaintext mnemonic bytes.
fn decrypt_mnemonic(envelope: &CryptoEnvelope) -> Result<SecretBytes, PayError> {
    let candidates: Vec<String> = match std::env::var("OWS_PASSPHRASE") {
        Ok(p) => vec![p],
        Err(_) => vec![DOCUMENTED_PASSPHRASE.to_string(), String::new()],
    };

    let mut last_err = None;
    for pass in &candidates {
        match decrypt(envelope, pass) {
            Ok(secret) => return Ok(secret),
            Err(e) => last_err = Some(e),
        }
    }
    Err(PayError::SignerError(format!(
        "failed to decrypt OWS vault with the configured passphrase(s): {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no passphrase candidates".into())
    )))
}

#[async_trait::async_trait]
impl Signer for ArcLocalSigner {
    async fn sign_hash(&self, hash: &B256) -> alloy::signers::Result<Signature> {
        self.inner.sign_hash(hash).await
    }

    fn address(&self) -> Address {
        self.address
    }

    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }

    fn set_chain_id(&mut self, chain_id: Option<ChainId>) {
        if let Some(id) = chain_id {
            self.chain_id = id;
            self.inner.set_chain_id(Some(id));
        }
    }
}
