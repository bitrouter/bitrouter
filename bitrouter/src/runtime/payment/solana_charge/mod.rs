//! Solana charge payment provider.
//!
//! Builds and broadcasts SPL token transfers (or native SOL transfers)
//! on the Solana network, returning the transaction signature as a
//! payment credential.
//!
//! Uses a minimal transaction builder (no `solana-sdk` dependency) with
//! OWS wallet signing. Transaction submission uses reqwest JSON-RPC.

use std::path::Path;

use mpp::client::PaymentProvider;
use mpp::error::MppError;
use mpp::protocol::core::{PaymentChallenge, PaymentCredential, PaymentPayload};
use mpp::protocol::intents::ChargeRequest;

use bitrouter_config::config::WalletConfig;

use self::tx::{Pubkey, SolanaTransaction};

mod tx;

/// Solana charge payment provider.
///
/// Signs and broadcasts SPL token transfers (or native SOL) to pay
/// upstream providers. The transaction signature is returned as the
/// payment credential.
#[derive(Clone)]
pub struct SolanaChargeProvider {
    wallet_name: String,
    credential: String,
    vault_path: Option<String>,
    payer: Pubkey,
    payer_b58: String,
    rpc_url: String,
    http_client: reqwest::Client,
}

impl SolanaChargeProvider {
    /// Create a new Solana charge provider from an OWS wallet.
    ///
    /// Resolves the wallet's Solana address without decrypting the key.
    pub fn new(wallet: &WalletConfig, credential: &str, rpc_url: &str) -> Result<Self, String> {
        let vault = wallet.vault_path.as_deref().map(Path::new);
        let info = ows_lib::get_wallet(&wallet.name, vault)
            .map_err(|e| format!("failed to load wallet '{}': {e}", wallet.name))?;

        let sol_account = info
            .accounts
            .iter()
            .find(|a| a.chain_id.starts_with("solana:"))
            .ok_or_else(|| format!("wallet '{}' has no Solana account", wallet.name))?;

        let payer_b58 = sol_account.address.clone();
        let payer_bytes = bs58::decode(&payer_b58)
            .into_vec()
            .map_err(|e| format!("invalid Solana address: {e}"))?;
        if payer_bytes.len() != 32 {
            return Err(format!(
                "Solana address must be 32 bytes, got {}",
                payer_bytes.len()
            ));
        }
        let mut payer = Pubkey([0u8; 32]);
        payer.0.copy_from_slice(&payer_bytes);

        Ok(Self {
            wallet_name: wallet.name.clone(),
            credential: credential.to_string(),
            vault_path: wallet.vault_path.clone(),
            payer,
            payer_b58,
            rpc_url: rpc_url.to_string(),
            http_client: reqwest::Client::new(),
        })
    }

    /// Sign a message with the Solana key from the OWS vault.
    ///
    /// Decrypts the key, signs, and zeroizes. Runs in a blocking thread
    /// because scrypt decryption is CPU-bound.
    async fn sign_message(&self, message: &[u8]) -> Result<[u8; 64], MppError> {
        let wallet_name = self.wallet_name.clone();
        let credential = self.credential.clone();
        let vault_path = self.vault_path.clone();
        let msg = message.to_vec();

        tokio::task::spawn_blocking(move || {
            let vault = vault_path.as_deref().map(std::path::Path::new);
            let key = ows_lib::decrypt_signing_key(
                &wallet_name,
                ows_core::ChainType::Solana,
                &credential,
                None,
                vault,
            )
            .map_err(|e| MppError::InvalidConfig(format!("decrypt Solana key: {e}")))?;

            let signer = ows_signer::signer_for_chain(ows_core::ChainType::Solana);
            let output = signer
                .sign(key.expose(), &msg)
                .map_err(|e| MppError::InvalidSignature(Some(format!("Ed25519 sign: {e}"))))?;

            if output.signature.len() != 64 {
                return Err(MppError::InvalidSignature(Some(format!(
                    "expected 64-byte signature, got {}",
                    output.signature.len()
                ))));
            }
            let mut sig = [0u8; 64];
            sig.copy_from_slice(&output.signature);
            Ok(sig)
        })
        .await
        .map_err(|e| MppError::Http(format!("blocking task: {e}")))?
    }

    /// Fetch the latest blockhash via JSON-RPC.
    async fn get_latest_blockhash(&self) -> Result<Pubkey, MppError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestBlockhash",
            "params": [{ "commitment": "confirmed" }]
        });

        let resp: serde_json::Value = self
            .http_client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MppError::Http(format!("RPC getLatestBlockhash: {e}")))?
            .json()
            .await
            .map_err(|e| MppError::Http(format!("RPC parse: {e}")))?;

        if let Some(err) = resp.get("error") {
            return Err(MppError::Http(format!(
                "RPC getLatestBlockhash error: {err}"
            )));
        }

        let hash_str = resp
            .pointer("/result/value/blockhash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MppError::Http("missing blockhash in RPC response".into()))?;

        let hash_bytes = bs58::decode(hash_str)
            .into_vec()
            .map_err(|e| MppError::Http(format!("invalid blockhash: {e}")))?;
        if hash_bytes.len() != 32 {
            return Err(MppError::Http(format!(
                "blockhash must be 32 bytes, got {}",
                hash_bytes.len()
            )));
        }
        let mut blockhash = Pubkey([0u8; 32]);
        blockhash.0.copy_from_slice(&hash_bytes);
        Ok(blockhash)
    }

    /// Submit a signed transaction and return the signature.
    async fn send_transaction(&self, signed_tx: &[u8]) -> Result<String, MppError> {
        let tx_b58 = bs58::encode(signed_tx).into_string();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                tx_b58,
                {
                    "encoding": "base58",
                    "skipPreflight": false,
                    "preflightCommitment": "confirmed"
                }
            ]
        });

        let resp: serde_json::Value = self
            .http_client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MppError::Http(format!("RPC sendTransaction: {e}")))?
            .json()
            .await
            .map_err(|e| MppError::Http(format!("RPC parse: {e}")))?;

        if let Some(err) = resp.get("error") {
            return Err(MppError::Http(format!("RPC sendTransaction error: {err}")));
        }

        let tx_sig = resp
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MppError::Http("missing result in sendTransaction".into()))?;

        Ok(tx_sig.to_string())
    }
}

impl PaymentProvider for SolanaChargeProvider {
    fn supports(&self, method: &str, intent: &str) -> bool {
        method == "solana" && intent == "charge"
    }

    async fn pay(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
        let charge_req: ChargeRequest = challenge
            .request
            .decode()
            .map_err(|e| MppError::InvalidConfig(format!("decode charge request: {e}")))?;

        let recipient_b58 = charge_req
            .recipient
            .as_deref()
            .ok_or_else(|| MppError::InvalidConfig("missing recipient".into()))?;
        let recipient = tx::pubkey_from_b58(recipient_b58)
            .map_err(|e| MppError::InvalidConfig(format!("invalid recipient: {e}")))?;

        let amount: u64 = charge_req
            .amount
            .parse()
            .map_err(|_| MppError::InvalidAmount(charge_req.amount.clone()))?;

        // Determine whether this is an SPL token or native SOL transfer.
        let mint_str = charge_req
            .method_details
            .as_ref()
            .and_then(|d| d.get("mint"))
            .and_then(|v| v.as_str());

        let is_spl = mint_str.is_some()
            || (charge_req.currency != "SOL" && tx::pubkey_from_b58(&charge_req.currency).is_ok());

        // Get recent blockhash.
        let blockhash = self.get_latest_blockhash().await?;

        let (message_bytes, tx_bytes) = if is_spl {
            let mint_addr = mint_str.unwrap_or(&charge_req.currency);
            let mint = tx::pubkey_from_b58(mint_addr)
                .map_err(|e| MppError::InvalidConfig(format!("invalid mint: {e}")))?;

            let decimals: u8 = charge_req
                .method_details
                .as_ref()
                .and_then(|d| d.get("decimals"))
                .and_then(|v| v.as_u64())
                .unwrap_or(6) as u8;

            SolanaTransaction::spl_transfer_checked(
                &self.payer,
                &recipient,
                &mint,
                amount,
                decimals,
                &blockhash,
            )?
        } else {
            SolanaTransaction::sol_transfer(&self.payer, &recipient, amount, &blockhash)?
        };

        // Sign the message and assemble the full transaction.
        let signature = self.sign_message(&message_bytes).await?;
        let signed_tx = SolanaTransaction::attach_signature(&tx_bytes, &signature);

        // Submit and get the transaction signature.
        let tx_sig = self.send_transaction(&signed_tx).await?;

        let echo = challenge.to_echo();
        let payload = PaymentPayload::hash(&tx_sig);
        let source = format!(
            "did:pkh:solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:{}",
            self.payer_b58
        );
        Ok(PaymentCredential::with_source(echo, source, payload))
    }
}
