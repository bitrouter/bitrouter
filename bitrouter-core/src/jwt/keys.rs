//! Multi-chain key management for BitRouter JWT authentication.
//!
//! A `MasterKeypair` wraps a 32-byte seed from which both Ed25519 (Solana)
//! and secp256k1 (EVM) keypairs are derived. Addresses are formatted per
//! CAIP-10 for cross-chain identity.
//!
//! Key storage: `BITROUTER_HOME/.keys/<prefix>/master.json`

use alloy_primitives::Address;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::jwt::JwtError;
use crate::jwt::chain::{Caip10, Chain};

/// A master keypair for signing BitRouter JWTs across chains.
///
/// Stores a 32-byte seed that deterministically derives:
/// - Ed25519 keypair (Solana)
/// - secp256k1 keypair (EVM)
#[derive(Clone)]
pub struct MasterKeypair {
    seed: [u8; 32],
}

impl MasterKeypair {
    /// Generate a new random 32-byte seed.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self {
            seed: signing_key.to_bytes(),
        }
    }

    /// Construct from a 32-byte seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self { seed }
    }

    /// Return the raw 32-byte seed.
    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    // ── Ed25519 (Solana) ──────────────────────────────────────

    /// Derive the Ed25519 signing key from the seed.
    pub fn ed25519_signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.seed)
    }

    /// Derive the Ed25519 verifying (public) key.
    pub fn ed25519_verifying_key(&self) -> VerifyingKey {
        self.ed25519_signing_key().verifying_key()
    }

    /// Solana public key as a base58-encoded string.
    pub fn solana_pubkey_b58(&self) -> String {
        bs58::encode(self.ed25519_verifying_key().as_bytes()).into_string()
    }

    // ── secp256k1 (EVM) ───────────────────────────────────────

    /// Construct an alloy `PrivateKeySigner` from the seed.
    pub fn evm_signer(&self) -> Result<PrivateKeySigner, JwtError> {
        PrivateKeySigner::from_slice(&self.seed).map_err(|e| JwtError::Secp256k1(e.to_string()))
    }

    /// Derive the EVM address (checksummed hex with `0x` prefix).
    pub fn evm_address(&self) -> Result<Address, JwtError> {
        Ok(self.evm_signer()?.address())
    }

    /// EVM address as a checksummed hex string (e.g. `"0xAb5801..."`).
    pub fn evm_address_string(&self) -> Result<String, JwtError> {
        Ok(self.evm_address()?.to_checksum(None))
    }

    // ── CAIP-10 ───────────────────────────────────────────────

    /// Derive the CAIP-10 account identifier for a given chain.
    pub fn caip10(&self, chain: &Chain) -> Result<Caip10, JwtError> {
        let address = match chain {
            Chain::Solana { .. } => self.solana_pubkey_b58(),
            Chain::Evm { .. } => self.evm_address_string()?,
        };
        Ok(Caip10 {
            chain: chain.clone(),
            address,
        })
    }

    // ── Display / prefix ──────────────────────────────────────

    /// A short prefix for display and directory naming.
    ///
    /// Uses the first 16 characters of the Solana base58 public key.
    pub fn public_key_prefix(&self) -> String {
        let b58 = self.solana_pubkey_b58();
        b58[..16.min(b58.len())].to_string()
    }

    // ── Signing helpers ───────────────────────────────────────

    /// Sign a byte slice using Ed25519 (Solana / SOL_EDDSA).
    ///
    /// Returns the 64-byte Ed25519 signature.
    pub fn sign_ed25519(&self, message: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let key = self.ed25519_signing_key();
        key.sign(message).to_vec()
    }

    /// Sign a byte slice using EIP-191 prefixed secp256k1 (EVM / EIP191K).
    ///
    /// The alloy signer applies the `"\x19Ethereum Signed Message:\n{len}"`
    /// prefix, hashes with keccak256, and signs with secp256k1 ECDSA.
    ///
    /// Returns the 65-byte signature (r[32] + s[32] + v[1]).
    pub fn sign_eip191(&self, message: &[u8]) -> Result<Vec<u8>, JwtError> {
        let signer = self.evm_signer()?;
        let sig = signer
            .sign_message_sync(message)
            .map_err(|e| JwtError::Signing(e.to_string()))?;
        Ok(sig.as_bytes().to_vec())
    }

    // ── Serialization ─────────────────────────────────────────

    /// Serialize to the JSON format stored in `master.json`.
    pub fn to_json(&self) -> MasterKeyJson {
        MasterKeyJson {
            algorithm: "web3".to_string(),
            seed: URL_SAFE_NO_PAD.encode(self.seed),
        }
    }

    /// Deserialize from the JSON format stored in `master.json`.
    pub fn from_json(json: &MasterKeyJson) -> Result<Self, JwtError> {
        if json.algorithm != "web3" {
            return Err(JwtError::InvalidKeypair);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&json.seed)
            .map_err(|_| JwtError::InvalidKeypair)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| JwtError::InvalidKeypair)?;
        Ok(Self::from_seed(seed))
    }
}

// ── Verification helpers (no private key needed) ──────────────

/// Decode a base58-encoded Solana public key into an Ed25519 `VerifyingKey`.
pub fn decode_solana_pubkey(b58: &str) -> Result<VerifyingKey, JwtError> {
    let bytes = bs58::decode(b58)
        .into_vec()
        .map_err(|_| JwtError::InvalidPublicKey)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| JwtError::InvalidPublicKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| JwtError::InvalidPublicKey)
}

/// JSON-serializable format for the master key file.
///
/// Stored at `BITROUTER_HOME/.keys/<prefix>/master.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterKeyJson {
    /// Algorithm identifier. `"web3"` for multi-chain seed.
    pub algorithm: String,
    /// The 32-byte seed, base64url-encoded (no padding).
    pub seed: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_seed() {
        let kp = MasterKeypair::generate();
        let seed = *kp.seed();
        let kp2 = MasterKeypair::from_seed(seed);
        assert_eq!(kp.solana_pubkey_b58(), kp2.solana_pubkey_b58());
    }

    #[test]
    fn roundtrip_json() {
        let kp = MasterKeypair::generate();
        let json = kp.to_json();
        let kp2 = MasterKeypair::from_json(&json).expect("valid json");
        assert_eq!(kp.solana_pubkey_b58(), kp2.solana_pubkey_b58());
    }

    #[test]
    fn same_seed_same_addresses() {
        let kp1 = MasterKeypair::generate();
        let kp2 = MasterKeypair::from_seed(*kp1.seed());
        assert_eq!(kp1.solana_pubkey_b58(), kp2.solana_pubkey_b58());
        assert_eq!(
            kp1.evm_address_string().expect("evm"),
            kp2.evm_address_string().expect("evm")
        );
    }

    #[test]
    fn solana_and_evm_addresses_differ() {
        let kp = MasterKeypair::generate();
        let sol = kp.solana_pubkey_b58();
        let evm = kp.evm_address_string().expect("evm");
        assert_ne!(sol, evm);
    }

    #[test]
    fn public_key_prefix_length() {
        let kp = MasterKeypair::generate();
        assert_eq!(kp.public_key_prefix().len(), 16);
    }

    #[test]
    fn caip10_solana() {
        let kp = MasterKeypair::generate();
        let chain = Chain::solana_mainnet();
        let id = kp.caip10(&chain).expect("caip10");
        assert!(
            id.format()
                .starts_with("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:")
        );
        assert_eq!(id.address, kp.solana_pubkey_b58());
    }

    #[test]
    fn caip10_evm() {
        let kp = MasterKeypair::generate();
        let chain = Chain::base();
        let id = kp.caip10(&chain).expect("caip10");
        assert!(id.format().starts_with("eip155:8453:0x"));
        assert_eq!(id.address, kp.evm_address_string().expect("evm"));
    }

    #[test]
    fn decode_solana_pubkey_roundtrip() {
        let kp = MasterKeypair::generate();
        let b58 = kp.solana_pubkey_b58();
        let vk = decode_solana_pubkey(&b58).expect("decode");
        assert_eq!(vk, kp.ed25519_verifying_key());
    }

    #[test]
    fn sign_ed25519_produces_64_bytes() {
        let kp = MasterKeypair::generate();
        let sig = kp.sign_ed25519(b"test message");
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn sign_eip191_produces_65_bytes() {
        let kp = MasterKeypair::generate();
        let sig = kp.sign_eip191(b"test message").expect("sign");
        assert_eq!(sig.len(), 65);
    }

    #[test]
    fn from_json_rejects_wrong_algorithm() {
        let kp = MasterKeypair::generate();
        let mut json = kp.to_json();
        json.algorithm = "rsa".to_string();
        assert!(MasterKeypair::from_json(&json).is_err());
    }

    #[test]
    fn eip191_signature_recovers_correct_address() {
        use alloy_primitives::Signature as EvmSignature;

        let kp = MasterKeypair::generate();
        let message = b"hello web3";
        let sig_bytes = kp.sign_eip191(message).expect("sign");

        let sig = EvmSignature::try_from(sig_bytes.as_slice()).expect("parse sig");
        let recovered = sig.recover_address_from_msg(message).expect("recover");
        assert_eq!(recovered, kp.evm_address().expect("address"));
    }
}
