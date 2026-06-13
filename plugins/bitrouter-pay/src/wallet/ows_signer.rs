//! OWS-backed signer for Arc testnet transactions.
//!
//! Signing is delegated to the `ows` CLI (`ows sign message --typed-data`) rather
//! than the OWS Rust library, because the library cannot resolve vault wallets by
//! name on this deployment. Private keys never leave the OWS vault: the CLI
//! decrypts, signs, and zeroizes in its own process. The Rust side only scans the
//! vault directory to resolve the wallet's EVM address.

use std::path::{Path, PathBuf};
use std::process::Command;

use alloy::consensus::SignableTransaction;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, ChainId, Signature, U256};
use alloy::signers::Signer;
use serde::Deserialize;

use crate::PayError;
use crate::chain::arc::{ARC_TESTNET_CAIP2, ARC_TESTNET_CHAIN_ID};

/// Hardcoded OWS CLI binary path (the npm shim on PATH is broken on this host).
const OWS_BIN_DEFAULT: &str = "/home/maka/.ows/bin/ows";

/// Minimal wallet record read from a vault JSON file.
#[derive(Debug, Deserialize)]
struct VaultWallet {
    name: String,
    accounts: Vec<VaultAccount>,
}

#[derive(Debug, Deserialize)]
struct VaultAccount {
    address: String,
    chain_id: String,
}

/// Structured output of `ows sign ... --json`.
#[derive(Debug, Deserialize)]
struct CliSignature {
    signature: String,
}

/// EVM signer backed by an OWS wallet on Arc testnet.
#[derive(Clone)]
pub struct ArcSigner {
    /// OWS wallet name used for CLI signing (e.g. `agent-treasury`).
    wallet_id: String,
    index: Option<u32>,
    vault_path: Option<PathBuf>,
    address: Address,
    chain_id: ChainId,
}

/// Default OWS vault directory: `$HOME/.ows/wallets`.
fn default_vault_dir() -> Result<PathBuf, PayError> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            PayError::SignerError("cannot determine home directory for OWS vault".into())
        })?;
    Ok(PathBuf::from(home).join(".ows").join("wallets"))
}

fn vault_dir(vault_path: Option<&Path>) -> Result<PathBuf, PayError> {
    if let Some(path) = vault_path {
        return Ok(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("OWS_VAULT_PATH") {
        return Ok(PathBuf::from(path));
    }
    default_vault_dir()
}

/// Resolve the `ows` CLI binary path.
///
/// Prefers `OWS_BIN` from the environment, then the hardcoded install path, then
/// falls back to `which ows`, then the bare `ows` name.
fn ows_binary() -> String {
    if let Ok(bin) = std::env::var("OWS_BIN")
        && !bin.is_empty()
    {
        return bin;
    }
    if Path::new(OWS_BIN_DEFAULT).exists() {
        return OWS_BIN_DEFAULT.to_string();
    }
    if let Ok(out) = Command::new("which").arg("ows").output()
        && out.status.success()
    {
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !resolved.is_empty() {
            return resolved;
        }
    }
    "ows".to_string()
}

/// Scan the vault for a wallet JSON whose `name` field matches `wallet_name`.
///
/// OWS stores wallets as `{uuid}.json`; the display name lives inside the file.
/// This verifies the wallet exists and yields its EVM address.
fn resolve_wallet_by_name(
    wallet_name: &str,
    vault_path: Option<&Path>,
) -> Result<VaultWallet, PayError> {
    let vault = vault_dir(vault_path)?;

    let entries = std::fs::read_dir(&vault).map_err(|e| {
        PayError::SignerError(format!(
            "failed to read OWS vault at {}: {e}",
            vault.display()
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| PayError::SignerError(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let contents = std::fs::read_to_string(&path).map_err(|e| {
            PayError::SignerError(format!(
                "failed to read wallet file {}: {e}",
                path.display()
            ))
        })?;

        let wallet: VaultWallet = serde_json::from_str(&contents).map_err(|e| {
            PayError::SignerError(format!("invalid wallet JSON in {}: {e}", path.display()))
        })?;

        if wallet.name == wallet_name {
            return Ok(wallet);
        }
    }

    Err(PayError::SignerError(format!(
        "wallet '{wallet_name}' not found in vault at {}",
        vault.display()
    )))
}

impl ArcSigner {
    /// Create a signer for `wallet_name`.
    ///
    /// Scans the vault to verify the wallet exists and resolve its EVM address,
    /// then caches the wallet name for OWS CLI signing. Prefers `OWS_WALLET_NAME`
    /// from the environment when set. Optionally reads `OWS_VAULT_PATH` to
    /// override the default vault directory.
    pub fn new(wallet_name: String) -> Result<Self, PayError> {
        let vault_path = std::env::var("OWS_VAULT_PATH").ok().map(PathBuf::from);

        let resolved_name = std::env::var("OWS_WALLET_NAME").unwrap_or(wallet_name);

        let wallet = resolve_wallet_by_name(&resolved_name, vault_path.as_deref())?;

        let evm_account = wallet
            .accounts
            .iter()
            .find(|a| a.chain_id.starts_with("eip155:"))
            .ok_or_else(|| {
                PayError::SignerError(format!(
                    "wallet '{resolved_name}' has no EVM account for {ARC_TESTNET_CAIP2}"
                ))
            })?;

        let address: Address = evm_account.address.parse().map_err(|e| {
            PayError::SignerError(format!("invalid EVM address {}: {e}", evm_account.address))
        })?;

        Ok(Self {
            wallet_id: resolved_name,
            index: None,
            vault_path,
            address,
            chain_id: ARC_TESTNET_CHAIN_ID,
        })
    }

    /// Sign EIP-712 typed data via the OWS CLI and return the 65-byte signature.
    ///
    /// Shells out to `ows sign message --typed-data <json> --json`, passing
    /// `OWS_VAULT_PATH` to the subprocess so the wallet resolves correctly.
    pub async fn sign_typed_data(&self, typed_data_json: &str) -> Result<Signature, PayError> {
        let wallet = self.wallet_id.clone();
        let index = self.index;
        let vault_path = self.vault_path.clone();
        let typed_data = typed_data_json.to_string();

        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(ows_binary());
            cmd.arg("sign")
                .arg("message")
                .arg("--chain")
                .arg(ARC_TESTNET_CAIP2)
                .arg("--wallet")
                .arg(&wallet)
                .arg("--message")
                .arg("")
                .arg("--typed-data")
                .arg(&typed_data)
                .arg("--json");
            if let Some(idx) = index {
                cmd.arg("--index").arg(idx.to_string());
            }
            if let Some(path) = vault_path.as_ref() {
                cmd.env("OWS_VAULT_PATH", path);
            }

            let output = cmd
                .output()
                .map_err(|e| PayError::SignerError(format!("failed to invoke ows CLI: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(PayError::SignerError(format!(
                    "ows sign exited with {}: {}",
                    output.status,
                    stderr.trim()
                )));
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_cli_signature(stdout.trim())
        })
        .await
        .map_err(|e| PayError::SignerError(format!("blocking task failed: {e}")))?
    }

    /// Resolved EVM address for this wallet.
    pub fn address(&self) -> Address {
        self.address
    }
}

/// Parse `{"signature":"<130-hex>"}` from `ows ... --json` into a [`Signature`].
fn parse_cli_signature(stdout: &str) -> Result<Signature, PayError> {
    let parsed: CliSignature = serde_json::from_str(stdout).map_err(|e| {
        PayError::SignerError(format!("invalid ows JSON output: {e} | output: {stdout}"))
    })?;

    let hex_str = parsed.signature.trim_start_matches("0x");
    let bytes = hex::decode(hex_str)
        .map_err(|e| PayError::SignerError(format!("invalid signature hex: {e}")))?;

    if bytes.len() != 65 {
        return Err(PayError::SignerError(format!(
            "expected 65-byte signature, got {}",
            bytes.len()
        )));
    }

    let r = B256::from_slice(&bytes[..32]);
    let s = B256::from_slice(&bytes[32..64]);
    // EIP-712 ecrecover v is 27 or 28; y_parity is true when v == 28.
    let y_parity = bytes[64] == 28 || bytes[64] == 1;

    Ok(Signature::new(
        U256::from_be_bytes(r.0),
        U256::from_be_bytes(s.0),
        y_parity,
    ))
}

#[async_trait::async_trait]
impl TxSigner<Signature> for ArcSigner {
    fn address(&self) -> Address {
        self.address()
    }

    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy::signers::Result<Signature> {
        let hash = tx.signature_hash();
        self.sign_hash(&hash).await
    }
}

#[async_trait::async_trait]
impl Signer for ArcSigner {
    async fn sign_hash(&self, _hash: &B256) -> alloy::signers::Result<Signature> {
        // The OWS CLI signs EIP-712 typed data, not bare 32-byte hashes. EVM
        // paywall flows must use `sign_typed_data`; raw-hash signing (used only by
        // the MPP on-chain path) is unsupported through the CLI.
        Err(alloy::signers::Error::other(
            "ArcSigner only supports EIP-712 typed-data signing via the OWS CLI; \
             use ArcSigner::sign_typed_data",
        ))
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
        }
    }
}
